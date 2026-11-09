//! ShtormTTS GUI. Кириллица: подключаем системный шрифт Segoe UI
//! (есть на любой Windows) с приоритетом — гарант корректного отображения.
//! Логика (форма/настройки/задание синтеза) — в библиотеке shtorm_gui.
//!
//! G4: диагностические режимы портативной сборки:
//! `--portable-check`, `--portable-selftest`, `--portable-selftest-batch`,
//! `--portable-selftest-cancel`. Они используют тот же
//! `execute_job`/`execute_batch`, что и GUI, и не заменяют интерактивную
//! проверку окна.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use shtorm_gui::job::{
    execute_batch, execute_job, resolve_model_dir, resolve_ort_dylib, JobConfig, JobEvent,
    JobStatus,
};
use shtorm_gui::resources;
use shtorm_gui::settings::{
    load_settings, settings_path, DeviceChoice, GuiSettings, OutputFormatChoice,
};

const FALLBACK_TEXT: &str =
    "Стр+елки час+ов показ+ывали п+олночь, а стрелк+и заст+ыли. Вт+орое предложение.";

fn main() -> eframe::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--portable-check") {
        std::process::exit(portable_check(&args));
    }
    if args.iter().any(|a| a == "--portable-selftest-cancel") {
        std::process::exit(portable_selftest_cancel(&args));
    }
    if args.iter().any(|a| a == "--portable-selftest-batch") {
        std::process::exit(portable_selftest(&args, true));
    }
    if args.iter().any(|a| a == "--portable-selftest") {
        std::process::exit(portable_selftest(&args, false));
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([980.0, 860.0])
            .with_min_inner_size([760.0, 560.0])
            .with_title("ShtormTTS — нативный GUI"),
        ..Default::default()
    };
    eframe::run_native(
        "ShtormTTS",
        options,
        Box::new(|cc| {
            install_cyrillic_fonts(&cc.egui_ctx);
            Ok(Box::new(shtorm_gui::app::ShtormApp::new()))
        }),
    )
}

fn arg_value<'a>(args: &'a [String], key: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == key)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

fn nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn load_portable_settings() -> Result<GuiSettings, String> {
    let path = settings_path();
    if path.is_file() {
        load_settings(&path).map_err(|e| format!("настройки {}: {e:#}", path.display()))
    } else {
        Ok(GuiSettings::default())
    }
}

fn apply_overrides(s: &mut GuiSettings, args: &[String]) {
    if let Some(dev) = arg_value(args, "--device") {
        s.device = if dev.eq_ignore_ascii_case("cuda") {
            DeviceChoice::Cuda
        } else {
            DeviceChoice::Cpu
        };
    }
    if let Some(fmt) = arg_value(args, "--format") {
        s.format = match fmt.to_ascii_lowercase().as_str() {
            "mp3" => OutputFormatChoice::Mp3,
            "opus" => OutputFormatChoice::Opus,
            _ => OutputFormatChoice::Wav,
        };
    }
    s.clamped();
}

fn hold_ms(args: &[String]) {
    if let Some(ms) = arg_value(args, "--hold-ms").and_then(|v| v.parse::<u64>().ok()) {
        println!("HOLDING: {ms} ms");
        std::thread::sleep(Duration::from_millis(ms));
    }
}

fn portable_check(args: &[String]) -> i32 {
    let mut s = match load_portable_settings() {
        Ok(s) => s,
        Err(e) => {
            println!("SETTINGS ERROR: {e}");
            return 2;
        }
    };
    apply_overrides(&mut s, args);
    let mut ok = true;
    println!("EXE: {}", resources::exe_dir().display());
    println!("SETTINGS: {}", settings_path().display());
    match resolve_model_dir(&s) {
        Ok(p) => println!("MODEL: {}", p.display()),
        Err(e) => {
            println!("MODEL ERROR: {e:#}");
            ok = false;
        }
    }
    match resolve_ort_dylib(&s) {
        Ok((dll, prepend)) => {
            println!("ORT DLL: {}", dll.display());
            for p in prepend {
                println!("ORT PATH: {}", p.display());
            }
        }
        Err(e) => {
            println!("ORT ERROR: {e:#}");
            ok = false;
        }
    }
    let explicit_ffmpeg = if s.ffmpeg_path.trim().is_empty() {
        None
    } else {
        Some(resources::resolve_user_path(&s.ffmpeg_path))
    };
    match app_core::sink::resolve_ffmpeg(explicit_ffmpeg.as_deref()) {
        Ok(p) => println!("FFMPEG: {}", p.display()),
        Err(e) => {
            println!("FFMPEG ERROR: {e:#}");
            ok = false;
        }
    }
    println!("MODEL CHOICE: {:?}", s.model);
    println!("DEVICE: {:?}", s.device);
    if ok {
        println!("PORTABLE CHECK OK");
        0
    } else {
        println!("PORTABLE CHECK FAILED");
        1
    }
}

fn portable_selftest(args: &[String], batch: bool) -> i32 {
    let mut s = match load_portable_settings() {
        Ok(s) => s,
        Err(e) => {
            println!("SETTINGS ERROR: {e}");
            return 2;
        }
    };
    apply_overrides(&mut s, args);
    let text = std::fs::read_to_string(resources::resolve_user_path(&s.text_file))
        .unwrap_or_else(|_| FALLBACK_TEXT.to_string());
    let stamp = format!("{}-{}", std::process::id(), nanos());
    let out_root = resources::resolve_user_path(&s.out_dir);
    if let Err(e) = std::fs::create_dir_all(&out_root) {
        println!("OUTPUT DIR ERROR: {}: {e}", out_root.display());
        return 2;
    }

    let result = if batch {
        let src = std::env::temp_dir().join(format!("shtorm_portable_batch_{stamp}"));
        if let Err(e) = std::fs::create_dir_all(src.join("sub")) {
            println!("BATCH SOURCE ERROR: {}: {e}", src.display());
            return 2;
        }
        if let Err(e) = std::fs::write(src.join("a.txt"), &text) {
            println!(
                "BATCH SOURCE WRITE ERROR: {}: {e}",
                src.join("a.txt").display()
            );
            return 2;
        }
        if let Err(e) = std::fs::write(src.join("sub").join("b.txt"), &text) {
            println!(
                "BATCH SOURCE WRITE ERROR: {}: {e}",
                src.join("sub").join("b.txt").display()
            );
            return 2;
        }
        s.out_dir = out_root
            .join(format!("batch-selftest-{stamp}"))
            .display()
            .to_string();
        let cancel = Arc::new(AtomicBool::new(false));
        println!("BATCH SOURCE: {}", src.display());
        println!("BATCH OUT: {}", s.out_dir);
        execute_batch(&s, &src, &cancel, &|_| {})
    } else {
        let out_path = out_root.join(format!("selftest-{stamp}.{}", s.format.extension_str()));
        let cfg = JobConfig {
            settings: s.clone(),
            text,
            out_path,
        };
        let cancel = AtomicBool::new(false);
        execute_job(&cfg, &cancel, &|_| {})
    };

    println!("STATUS: {}", result.status.label());
    println!("SUMMARY: {}", result.summary);
    if let Some(p) = &result.final_path {
        println!("OUTPUT: {}", p.display());
    }
    if let Some(p) = &result.partial_path {
        println!("PARTIAL: {}", p.display());
    }
    if let Some(r) = &result.batch {
        println!("BATCH: {}", r.summary());
        for line in r.detail_lines() {
            println!("FILE: {line}");
        }
    }
    let _ = std::io::Write::flush(&mut std::io::stdout());
    hold_ms(args);
    if result.status == JobStatus::Completed {
        0
    } else {
        1
    }
}

/// G4/P2A.1: диагностика Stop/Cancel на портативном exe. Текст утраивается,
/// чтобы чанков было заведомо несколько; отмена ставится после первого
/// готового чанка (воркер в этот момент уже генерирует следующий). Проверка:
/// Cancelled без итогового файла, `.partial` существует; повторный запуск на
/// том же пути — Completed. Возвращает 0 только при обеих проверках.
fn portable_selftest_cancel(args: &[String]) -> i32 {
    let mut s = match load_portable_settings() {
        Ok(s) => s,
        Err(e) => {
            println!("SETTINGS ERROR: {e}");
            return 2;
        }
    };
    apply_overrides(&mut s, args);
    let base = std::fs::read_to_string(resources::resolve_user_path(&s.text_file))
        .unwrap_or_else(|_| FALLBACK_TEXT.to_string());
    let text = format!("{base} {base} {base}");
    let stamp = format!("{}-{}", std::process::id(), nanos());
    let out_root = resources::resolve_user_path(&s.out_dir);
    if let Err(e) = std::fs::create_dir_all(&out_root) {
        println!("OUTPUT DIR ERROR: {}: {e}", out_root.display());
        return 2;
    }
    let out_path = out_root.join(format!(
        "cancel-selftest-{stamp}.{}",
        s.format.extension_str()
    ));
    let cfg = JobConfig {
        settings: s.clone(),
        text,
        out_path: out_path.clone(),
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let c2 = Arc::clone(&cancel);
    let first = execute_job(&cfg, &cancel, &|ev| {
        if let JobEvent::Progress { done, .. } = &ev {
            if *done >= 1 {
                c2.store(true, Ordering::SeqCst);
            }
        }
    });
    println!("STATUS: {}", first.status.label());
    println!("SUMMARY: {}", first.summary);
    if let Some(p) = &first.partial_path {
        println!("PARTIAL: {}", p.display());
    }
    let cancel_ok = first.status == JobStatus::Cancelled
        && first.final_path.is_none()
        && first.partial_path.as_ref().is_some_and(|p| p.is_file());

    let second = execute_job(
        &JobConfig {
            settings: cfg.settings.clone(),
            text: cfg.text.clone(),
            out_path,
        },
        &Arc::new(AtomicBool::new(false)),
        &|_| {},
    );
    println!("RERUN STATUS: {}", second.status.label());
    println!("RERUN SUMMARY: {}", second.summary);
    if let Some(p) = &second.final_path {
        println!("RERUN OUTPUT: {}", p.display());
    }
    let rerun_ok = second.status == JobStatus::Completed && second.final_path.is_some();

    let _ = std::io::Write::flush(&mut std::io::stdout());
    if cancel_ok && rerun_ok {
        println!("CANCEL SELFTEST OK");
        0
    } else {
        println!("CANCEL SELFTEST FAILED (cancel_ok={cancel_ok}, rerun_ok={rerun_ok})");
        1
    }
}

fn install_cyrillic_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    // Segoe UI содержит кириллицу; берём системную копию
    for cand in [
        r"C:\Windows\Fonts\segoeui.ttf",
        r"C:\Windows\Fonts\tahoma.ttf",
        r"C:\Windows\Fonts\arial.ttf",
    ] {
        if let Ok(bytes) = std::fs::read(cand) {
            fonts
                .font_data
                .insert("system_ui".into(), egui::FontData::from_owned(bytes).into());
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .insert(0, "system_ui".into());
            fonts
                .families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .push("system_ui".into());
            break;
        }
    }
    ctx.set_fonts(fonts);
}

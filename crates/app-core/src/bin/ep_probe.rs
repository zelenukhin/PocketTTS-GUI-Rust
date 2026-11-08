//! ep_probe — проверка достоверности CUDA-режима (Часть 1 задачи).
//!
//! A. TF32-проба: три сессии графа flow_lm_flow (опция не задана / use_tf32=0 /
//!    use_tf32=1) на одинаковых входах; побитовое сравнение показывает,
//!    (1) меняет ли TF32 численный результат на этом железе,
//!    (2) совпадает ли «опция не задана» с use_tf32=0 (дефолт выключен) или с =1.
//! B. Распределение операторов: полная генерация с профилированием ORT
//!    (Chrome trace, по файлу на сессию); считаем события узлов по
//!    ExecutionProvider'ам. Профилирование замедляет запуск — это диагностика.

use anyhow::{bail, Context, Result};
use clap::Parser;
use ndarray::Array;
use ort::session::Session;
use ort::value::Tensor;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(about = "CUDA EP probe: TF32 default + per-op provider distribution")]
struct Args {
    #[arg(long)]
    model_dir: PathBuf,
    #[arg(long)]
    voice: String,
    #[arg(
        long,
        default_value = "Стр+елки час+ов показ+ывали п+олночь, а стрелк+и заст+ыли на пост+ах."
    )]
    text: String,
    #[arg(long, default_value_t = 0)]
    device_id: i32,
    /// Куда писать трассировки (создаётся)
    #[arg(long, default_value = "out/ep_traces")]
    trace_dir: PathBuf,
}

/// Одна прогонка flow-графа на фиксированных входах; возвращаем flow_dir.
fn run_flow(session: &mut Session) -> Result<Vec<f32>> {
    let c: Vec<f32> = (0..1024)
        .map(|i| (((i * 37) % 97) as f32) / 97.0 - 0.5)
        .collect();
    let x: Vec<f32> = (0..32)
        .map(|i| (((i * 53) % 89) as f32) / 89.0 - 0.5)
        .collect();
    let out = session.run(ort::inputs! {
        "c" => Tensor::from_array(Array::from_shape_vec(1024, c)?)?,
        "s" => Tensor::from_array(Array::from_shape_vec((1, 1), vec![0.0f32])?)?,
        "t" => Tensor::from_array(Array::from_shape_vec((1, 1), vec![1.0f32])?)?,
        "x" => Tensor::from_array(Array::from_shape_vec((1, 32), x)?)?,
    })?;
    Ok(out["flow_dir"].try_extract_tensor::<f32>()?.1.to_vec())
}

fn compare(name: &str, a: &[f32], b: &[f32]) {
    let same_bits = a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.to_bits() == y.to_bits());
    let maxdiff = a
        .iter()
        .zip(b)
        .map(|(x, y)| (x - y).abs())
        .fold(0.0f32, f32::max);
    println!("  {name}: bitwise_equal={same_bits} max_abs_diff={maxdiff:.3e}");
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let args = Args::parse();
    if std::env::var("ORT_DYLIB_PATH").is_err() {
        bail!("ORT_DYLIB_PATH не задан (нужна GPU-сборка onnxruntime)");
    }

    // ---------- A. TF32 ----------
    println!("== A. TF32 probe (flow_lm_flow [Gemm-heavy] + mimi_encoder [Conv-heavy], фиксированные входы) ==");
    use ort::ep::{CPU, CUDA};
    let flow_path = args.model_dir.join("flow_lm_flow.onnx");
    let build = |tf32: Option<bool>, path: &std::path::Path| -> Result<Session> {
        let ep = match tf32 {
            None => CUDA::default().with_device_id(args.device_id).build(),
            Some(false) => CUDA::default()
                .with_device_id(args.device_id)
                .with_tf32(false)
                .build(),
            Some(true) => CUDA::default()
                .with_device_id(args.device_id)
                .with_tf32(true)
                .build(),
        };
        Ok(Session::builder()?
            .with_execution_providers([ep, CPU::default().build()])?
            .commit_from_file(path)?)
    };
    let mut s_unset = build(None, &flow_path).context("load flow (unset)")?;
    let mut s_off = build(Some(false), &flow_path).context("load flow (tf32=0)")?;
    let mut s_on = build(Some(true), &flow_path).context("load flow (tf32=1)")?;
    // прогрев (аллокации/компиляция ядер), затем по 3 прогона на сессию
    let _ = run_flow(&mut s_unset)?;
    let _ = run_flow(&mut s_off)?;
    let _ = run_flow(&mut s_on)?;
    let r_unset = run_flow(&mut s_unset)?;
    let r_off = run_flow(&mut s_off)?;
    let r_on = run_flow(&mut s_on)?;
    println!("flow_lm_flow (Gemm):");
    println!("  unset vs use_tf32=0:");
    compare("unset_off", &r_unset, &r_off);
    println!("  unset vs use_tf32=1:");
    compare("unset_on", &r_unset, &r_on);
    println!("  tf32=0 vs tf32=1:");
    compare("off_on", &r_off, &r_on);

    // Свёрточный граф (mimi_encoder; вход — просто audio, без стейтов):
    // TF32 в ONNX Runtime применяется прежде всего к cuDNN-свёрткам.
    let enc_path = args.model_dir.join("mimi_encoder.onnx");
    let run_enc = |s: &mut Session| -> Result<Vec<f32>> {
        let n = 24000usize;
        let a: Vec<f32> = (0..n)
            .map(|i| (((i * 31) % 101) as f32 / 101.0 - 0.5) * 0.8)
            .collect();
        let out = s.run(ort::inputs! {
            "audio" => Tensor::from_array(Array::from_shape_vec((1, 1, n), a)?)?,
        })?;
        Ok(out["conditioning"].try_extract_tensor::<f32>()?.1.to_vec())
    };
    let mut e_unset = build(None, &enc_path).context("load encoder (unset)")?;
    let mut e_off = build(Some(false), &enc_path).context("load encoder (tf32=0)")?;
    let mut e_on = build(Some(true), &enc_path).context("load encoder (tf32=1)")?;
    let _ = run_enc(&mut e_unset)?;
    let _ = run_enc(&mut e_off)?;
    let _ = run_enc(&mut e_on)?;
    let er_unset = run_enc(&mut e_unset)?;
    let er_off = run_enc(&mut e_off)?;
    let er_on = run_enc(&mut e_on)?;
    println!("mimi_encoder (Conv):");
    println!("  unset vs use_tf32=0:");
    compare("enc_unset_off", &er_unset, &er_off);
    println!("  unset vs use_tf32=1:");
    compare("enc_unset_on", &er_unset, &er_on);
    println!("  tf32=0 vs tf32=1:");
    compare("enc_off_on", &er_off, &er_on);

    let flow_ident = r_unset
        .iter()
        .zip(&r_off)
        .all(|(x, y)| x.to_bits() == y.to_bits())
        && r_unset
            .iter()
            .zip(&r_on)
            .all(|(x, y)| x.to_bits() == y.to_bits());
    let enc_diff_on = er_off
        .iter()
        .zip(&er_on)
        .any(|(x, y)| x.to_bits() != y.to_bits());
    let enc_default_is_on = er_unset
        .iter()
        .zip(&er_on)
        .all(|(x, y)| x.to_bits() == y.to_bits())
        && enc_diff_on;
    let verdict = if enc_default_is_on {
        "DEFAULT==TF32_ON для conv-графа: дефолт провайдера ВКЛЮЧАЕТ TF32 — явное use_tf32=0 обязательно (сделано в session_builder)"
    } else if enc_diff_on {
        "TF32 меняет численный результат conv-графа; дефолт совпадает с tf32=0 (выключен)"
    } else if flow_ident {
        "TF32 не влияет на численный результат этих графов на данном железе/сборке ORT (и дефолт выключен); use_tf32=0 зафиксирован явно"
    } else {
        "СМЕШАННЫЙ результат — смотреть сырые сравнения выше"
    };
    println!("VERDICT A: {verdict}");

    // ---------- B. Provider distribution ----------
    println!("\n== B. Профилирование: распределение операторов (Chrome trace) ==");
    // Очистка только собственного каталога трасс: гвард в app_core::trace_clean
    // (канонизация обеих сторон, защищённые пути, белый список содержимого).
    // Урок инцидента 2026-11-08: remove_dir_all по пользовательскому пути без
    // гварда удалил скрытые каталоги профиля — больше здесь нет прямых удалений.
    app_core::trace_clean::clean_trace_dir(&args.trace_dir)?;
    use shtorm_engine::{LoadOptions, Pocket};
    let mut engine = Pocket::load(
        &args.model_dir,
        &LoadOptions {
            device: shtorm_engine::Device::cuda(args.device_id),
            threads: 4,
            seed: Some(42),
            profile_dir: Some(args.trace_dir.clone()),
        },
    )?;
    let text = text_processing::to_model_stress_notation(&args.text);
    let audio = engine.say_chunk(&text, &args.voice, 0)?;
    println!(
        "сгенерировано {:.2} с аудио (с профилированием — медленнее обычного)",
        audio.len() as f32 / shtorm_engine::POCKET_RATE as f32
    );
    let files = engine.end_profiling();
    drop(engine);

    let mut total_per_provider: BTreeMap<String, usize> = BTreeMap::new();
    for f in &files {
        let raw = std::fs::read_to_string(f).with_context(|| format!("read {f}"))?;
        let v: serde_json::Value = serde_json::from_str(&raw)?;
        // Chrome trace: массив событий; узлы имеют "cat":"Node" (или "Op") и
        // args.provider. Считаем устойчиво: любое событие с args.provider.
        let mut per_provider: BTreeMap<String, usize> = BTreeMap::new();
        let mut ops_on_cpu: BTreeMap<String, usize> = BTreeMap::new();
        if let Some(events) = v.as_array() {
            for e in events {
                let args = &e["args"];
                let prov = args["provider"].as_str().unwrap_or("").to_string();
                if prov.is_empty() {
                    continue;
                }
                *per_provider.entry(prov.clone()).or_default() += 1;
                if prov.contains("CPU") {
                    let op = args["op_name"].as_str().unwrap_or("?").to_string();
                    *ops_on_cpu.entry(op).or_default() += 1;
                }
            }
        }
        let name = std::path::Path::new(f)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let total: usize = per_provider.values().sum();
        let parts: Vec<String> = per_provider
            .iter()
            .map(|(k, c)| format!("{k}: {c}"))
            .collect();
        println!("  {name}: узлов {total} | {}", parts.join(", "));
        if !ops_on_cpu.is_empty() {
            let top: Vec<String> = ops_on_cpu.iter().map(|(k, c)| format!("{k}x{c}")).collect();
            println!("    на CPU: {}", top.join(", "));
        }
        for (k, c) in per_provider {
            *total_per_provider.entry(k).or_default() += c;
        }
    }
    println!(
        "ИТОГО по провайдерам (все сессии): {:?}",
        total_per_provider
    );
    Ok(())
}

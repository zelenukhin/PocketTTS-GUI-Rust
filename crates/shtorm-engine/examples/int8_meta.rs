//! G6.3a: INT8-диагностика — загрузка FP32/INT8-графов через тот же билдер
//! сессий, что в `Pocket::load` (Level3, CUDA+CPU или CPU), и сверка
//! интерфейсов сессии. Несовместимый оператор = ошибка создания сессии
//! («not supported by any EP»), поэтому успешная загрузка — фактическая
//! проверка поддержки EP, а не предположение.
//!
//! Запуск:
//!   ORT_DYLIB_PATH=... cargo run --release -p shtorm-engine --example \
//!     int8_meta -- <model_dir> [--cpu|--cuda] [--out FILE]

use anyhow::{Context, Result};
use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::ValueType;
use std::io::Write;
use std::path::PathBuf;

fn describe(io: &ort::value::Outlet) -> String {
    match io.dtype() {
        ValueType::Tensor { ty, shape, .. } => {
            let dims: Vec<String> = shape.iter().map(|d| d.to_string()).collect();
            format!("Tensor<{ty:?}> [{}]", dims.join(", "))
        }
        other => format!("{other:?}"),
    }
}

fn load(dir: &std::path::Path, stem: &str, cpu: bool) -> Result<Session> {
    let path = dir.join(format!("{stem}.onnx"));
    let mut builder = Session::builder()?
        .with_optimization_level(GraphOptimizationLevel::Level3)?
        .with_intra_threads(4)?;
    builder = if cpu {
        use ort::ep::CPU;
        builder.with_execution_providers([CPU::default().build()])?
    } else {
        use ort::ep::{CPU, CUDA};
        builder.with_execution_providers([
            CUDA::default().with_device_id(0).with_tf32(false).build(),
            CPU::default().build(),
        ])?
    };
    builder
        .commit_from_file(&path)
        .with_context(|| format!("load {}", path.display()))
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let dir = PathBuf::from(
        args.next()
            .context("usage: int8_meta <model_dir> [--cpu|--cuda] [--out FILE]")?,
    );
    let cpu = match args.next().as_deref() {
        Some("--cpu") => true,
        Some("--cuda") => false,
        other => anyhow::bail!("ожидается --cpu или --cuda, получено {other:?}"),
    };
    let out_path = match args.next().as_deref() {
        Some("--out") => Some(PathBuf::from(args.next().context("--out требует путь")?)),
        Some(other) => anyhow::bail!("неизвестный аргумент {other}"),
        None => None,
    };
    // Три пары FP32/INT8 + два всегда-FP32 графа (INT8-версий не существует).
    let pairs: &[(&str, &str)] = &[
        ("flow_lm_main", "flow_lm_main_int8"),
        ("flow_lm_flow", "flow_lm_flow_int8"),
        ("mimi_decoder", "mimi_decoder_int8"),
    ];
    let fp32_only = ["mimi_encoder", "text_conditioner"];

    let mut w: Box<dyn Write> = match out_path {
        Some(p) => {
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&p)
                .with_context(|| format!("--out {} (перезапись запрещена)", p.display()))?;
            Box::new(file)
        }
        None => Box::new(std::io::stdout()),
    };
    let ep = if cpu { "CPU" } else { "CUDA+CPU" };
    for (fp32, int8) in pairs {
        for stem in [*fp32, *int8] {
            let t = std::time::Instant::now();
            match load(&dir, stem, cpu) {
                Ok(s) => {
                    let line = format!(
                        "{stem}: OK via {ep} ({} ms), inputs={} outputs={}",
                        t.elapsed().as_millis(),
                        s.inputs().len(),
                        s.outputs().len()
                    );
                    writeln!(w, "{line}")?;
                    writeln!(w, "  inputs:")?;
                    for i in s.inputs() {
                        writeln!(w, "    {:<22} {}", i.name(), describe(i))?;
                    }
                    writeln!(w, "  outputs:")?;
                    for o in s.outputs() {
                        writeln!(w, "    {:<22} {}", o.name(), describe(o))?;
                    }
                }
                Err(e) => {
                    writeln!(w, "{stem}: FAIL via {ep}: {e:#}")?;
                }
            }
        }
    }
    for stem in fp32_only {
        match load(&dir, stem, cpu) {
            Ok(_) => writeln!(w, "{stem}: OK via {ep} (always-FP32 baseline)")?,
            Err(e) => writeln!(w, "{stem}: FAIL via {ep}: {e:#}")?,
        }
    }
    w.flush()?;
    Ok(())
}

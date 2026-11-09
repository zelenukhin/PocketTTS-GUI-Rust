//! graph_meta — диагностика CUDA-P2A: печатает имена, типы и формы входов/выходов
//! ONNX-графов (main/flow/decoder), чтобы сверить state_* ↔ out_state_* по
//! действительным метаданным сессии, а не по позиции в списке.
//!
//! Запуск (CUDA-сессия, как в движке):
//!   ORT_DYLIB_PATH=... SHTORM_GRAPH_META=out/p2a/meta.txt \
//!     cargo run --release -p shtorm-engine --example graph_meta -- <model_dir> [--cpu]
//!
//! Вывод также пишется в файл, если задан SHTORM_GRAPH_META (out/ — диагностика).

use anyhow::{Context, Result};
use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::ValueType;
use std::io::Write;
use std::path::{Path, PathBuf};

fn describe(io: &ort::value::Outlet) -> String {
    match io.dtype() {
        ValueType::Tensor { ty, shape, .. } => {
            let dims: Vec<String> = shape.iter().map(|d| d.to_string()).collect();
            format!("Tensor<{ty:?}> [{}]", dims.join(", "))
        }
        other => format!("{other:?}"),
    }
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let dir = PathBuf::from(
        args.next()
            .context("usage: graph_meta <model_dir> [--cpu]")?,
    );
    let cpu = args.any(|a| a == "--cpu");
    let graphs = [
        "mimi_encoder",
        "text_conditioner",
        "flow_lm_main",
        "flow_lm_flow",
        "mimi_decoder",
    ];

    let mut out: Box<dyn Write> = match std::env::var("SHTORM_GRAPH_META") {
        Ok(p) => {
            let p = Path::new(&p);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            // Диагностика не перезаписывает существующие файлы: только
            // эксклюзивное создание по уникальному пути (SAFETY_AUDIT).
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(p)
                .with_context(|| {
                    format!(
                        "SHTORM_GRAPH_META: {} уже существует (перезапись запрещена) \
                         или не может быть создан; задайте уникальный путь",
                        p.display()
                    )
                })?;
            Box::new(file)
        }
        Err(_) => Box::new(std::io::stdout()),
    };

    use ort::ep::{CPU, CUDA};
    for name in graphs {
        let path = dir.join(format!("{name}.onnx"));
        let mut builder = Session::builder()?
            .with_optimization_level(GraphOptimizationLevel::Level3)?
            .with_intra_threads(4)?;
        builder = if cpu {
            builder.with_execution_providers([CPU::default().build()])?
        } else {
            builder.with_execution_providers([
                CUDA::default().with_device_id(0).with_tf32(false).build(),
                CPU::default().build(),
            ])?
        };
        let session = builder
            .commit_from_file(&path)
            .with_context(|| format!("load {}", path.display()))?;
        writeln!(out, "== {name} ==")?;
        writeln!(out, "  inputs:")?;
        for i in session.inputs() {
            writeln!(out, "    {:<20} {}", i.name(), describe(i))?;
        }
        writeln!(out, "  outputs:")?;
        for o in session.outputs() {
            writeln!(out, "    {:<20} {}", o.name(), describe(o))?;
        }
    }
    out.flush()?;
    Ok(())
}

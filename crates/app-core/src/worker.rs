//! Реальный воркер пула: владеет собственным `Pocket` и выполняет DSP-цепочку
//! A3 внутри своего потока. Параметры задания идентичны для всех воркеров
//! (их задаёт фабрика из одного PipelineOptions + base_seed).

use anyhow::Result;
use std::path::PathBuf;

use shtorm_engine::dsp::{fade_out, trim_edges};
use shtorm_engine::{high_shelf, resample, Device, LoadOptions, Pocket, POCKET_RATE};

use crate::pipeline::{resolve_denoise, PipelineOptions};
use crate::pool::{ChunkAudio, ChunkWorker, DenoiseChunkStatus, Task};
use shtorm_engine::denoise as denoise_mod;

pub struct RealWorker {
    engine: Pocket,
    voice: String,
    brightness_db: f32,
    /// Очистка запрошена заданием, но файла модели нет: каждый чанк получает
    /// статус Missing (не молчаливый Off) — A4-фикс 1.3.
    denoise_missing: bool,
}

/// Фабрика воркеров из параметров задания: каждый вызов загружает СВОЙ
/// экземпляр Pocket (seed = base_seed — общий для задания), выставляет
/// параметры и настройку денойза (файл модели проверен заранее).
pub fn real_worker_factory(
    opts: &PipelineOptions,
    base_seed: u64,
) -> impl Fn(usize, u64) -> Result<RealWorker> + Send + Sync + use<> {
    let model_dir: PathBuf = opts.model_dir.clone();
    let voice = opts.voice.to_string_lossy().to_string();
    let temperature = opts.temperature;
    let decode_steps = opts.decode_steps;
    let eos_threshold = opts.eos_threshold;
    let threads = opts.threads;
    let brightness_db = opts.brightness_db;
    let device: Device = opts.device.clone();
    let denoise_requested = opts.denoise;
    let anti_repeat = opts.anti_repeat;
    move |_worker_id: usize, _seed: u64| {
        let mut engine = Pocket::load(
            &model_dir,
            &LoadOptions {
                device: device.clone(),
                threads,
                seed: Some(base_seed), // seed+index: детерминизм от номера воркера не зависит
                profile_dir: None,
            },
        )?;
        engine.temperature = temperature;
        engine.decode_steps = decode_steps;
        engine.eos_threshold = eos_threshold;
        engine.anti_repeat = anti_repeat;
        let denoise_model = model_dir.join(denoise_mod::MODEL_FILE);
        let model_present = denoise_model.exists();
        engine.denoise_on = resolve_denoise(denoise_requested, model_present);
        // A4-фикс 1.3: при отсутствии модели каждый чанк получает явный
        // статус Missing в параллельном/последовательном путях.
        let denoise_missing =
            crate::pipeline::denoise_precheck(denoise_requested, model_present).is_some();
        Ok(RealWorker {
            engine,
            voice: voice.clone(),
            brightness_db,
            denoise_missing,
        })
    }
}

impl ChunkWorker for RealWorker {
    fn process(&mut self, task: &Task) -> Result<ChunkAudio> {
        let audio = self
            .engine
            .say_chunk(&task.model_text, &self.voice, task.index)?;
        let mut piece = audio;
        if self.brightness_db > 0.05 {
            high_shelf(&mut piece, self.brightness_db, 3000.0, POCKET_RATE as f32);
        }
        let denoise = if self.denoise_missing {
            DenoiseChunkStatus::Missing
        } else if self.engine.denoise_on {
            match self.engine.clean(&piece) {
                shtorm_engine::CleanOutcome::Applied(clean) => {
                    let back = resample(&clean, denoise_mod::RATE, POCKET_RATE);
                    piece = back[..back.len().min(piece.len())].to_vec();
                    DenoiseChunkStatus::Applied
                }
                shtorm_engine::CleanOutcome::ModelMissing => DenoiseChunkStatus::Missing,
                shtorm_engine::CleanOutcome::Off => DenoiseChunkStatus::Off,
                shtorm_engine::CleanOutcome::LoadFailed(e)
                | shtorm_engine::CleanOutcome::RunFailed(e) => DenoiseChunkStatus::Failed(e),
            }
        } else {
            DenoiseChunkStatus::Off
        };
        let mut piece = trim_edges(&piece, 0.02, 0.10, POCKET_RATE);
        fade_out(&mut piece, POCKET_RATE);
        Ok(ChunkAudio {
            speech: piece,
            pause_s: task.pause_s,
            denoise,
        })
    }
}

//! Реальный воркер пула: владеет собственным `Pocket` и выполняет DSP-цепочку
//! A3 внутри своего потока. Параметры задания идентичны для всех воркеров
//! (их задаёт фабрика из одного PipelineOptions + base_seed).

use anyhow::Result;
use std::path::PathBuf;

use shtorm_engine::dsp::{fade_out, trim_edges};
use shtorm_engine::{
    high_shelf, resample, Device, EosTail, LoadOptions, ModelPrecision, Pocket, POCKET_RATE,
};

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

impl RealWorker {
    /// G6.1: per-file seed на ПОСТОЯННОМ Pocket. `Pocket::say_chunk`
    /// пересеивает RNG как `seed + chunk_index` на каждый вызов, поэтому
    /// установка поля перед первым чанком файла даёт поток, идентичный
    /// свежему Pocket, загруженному с этим seed (побитовый паритет
    /// переиспользования Pocket доказан P2B.2 §11; см. persistent.rs).
    pub fn set_seed(&mut self, seed: u64) {
        self.engine.seed = Some(seed);
    }
}

/// Параметры генерации, снимаемые с `PipelineOptions` на КАЖДЫЙ экземпляр
/// Pocket (G4.1): воркеры одного задания получают идентичные значения.
/// Выделено в структуру для регрессионного теста без ONNX — сам Pocket
/// загрузить без моделей нельзя, а соответствие opts → движок проверяемо.
#[derive(Debug, Clone, PartialEq)]
pub struct EngineParams {
    pub temperature: f32,
    pub decode_steps: usize,
    pub eos_threshold: f32,
    pub eos_tail_short: EosTail,
    pub eos_tail_long: EosTail,
    pub anti_repeat: bool,
}

/// Снять параметры движка с параметров задания (одинаково для всех воркеров,
/// независимо от их номера и количества).
pub fn engine_params(opts: &PipelineOptions) -> EngineParams {
    EngineParams {
        temperature: opts.temperature,
        decode_steps: opts.decode_steps,
        eos_threshold: opts.eos_threshold,
        eos_tail_short: opts.eos_tail_short,
        eos_tail_long: opts.eos_tail_long,
        anti_repeat: opts.anti_repeat,
    }
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
    let params = engine_params(opts);
    let threads = opts.threads;
    let brightness_db = opts.brightness_db;
    let device: Device = opts.device.clone();
    let precision: ModelPrecision = opts.precision;
    let denoise_requested = opts.denoise;
    move |_worker_id: usize, _seed: u64| {
        let mut engine = Pocket::load(
            &model_dir,
            &LoadOptions {
                device: device.clone(),
                threads,
                seed: Some(base_seed), // seed+index: детерминизм от номера воркера не зависит
                precision,
                profile_dir: None,
            },
        )?;
        engine.temperature = params.temperature;
        engine.decode_steps = params.decode_steps;
        engine.eos_threshold = params.eos_threshold;
        engine.eos_tail_short = params.eos_tail_short;
        engine.eos_tail_long = params.eos_tail_long;
        engine.anti_repeat = params.anti_repeat;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::PipelineOptions;
    use shtorm_engine::Device;

    fn opts() -> PipelineOptions {
        PipelineOptions {
            precision: ModelPrecision::Fp32,
            model_dir: PathBuf::from(r"C:\m"),
            voice: PathBuf::from(r"C:\v.wav"),
            device: Device::Cpu,
            threads: 4,
            seed: Some(42),
            temperature: 0.3,
            decode_steps: 1,
            eos_threshold: -4.0,
            eos_tail_short: EosTail::Fixed(5),
            eos_tail_long: EosTail::Fixed(3),
            brightness_db: 8.0,
            denoise: false,
            pause_scale: 1.0,
            paragraph_pause_s: 0.9,
            max_tokens: 50,
            char_rules: None,
            anti_repeat: true,
        }
    }

    /// G4.1: PipelineOptions → параметры движка переносятся без потерь,
    /// включая обе ветки хвоста EOS и порог. Чистая функция — ONNX не нужен.
    #[test]
    fn engine_params_mirror_options() {
        let o = opts();
        let p = engine_params(&o);
        assert_eq!(p.temperature, 0.3);
        assert_eq!(p.decode_steps, 1);
        assert_eq!(p.eos_threshold, -4.0);
        assert_eq!(p.eos_tail_short, EosTail::Fixed(5));
        assert_eq!(p.eos_tail_long, EosTail::Fixed(3));
        assert!(p.anti_repeat);
    }

    /// G4.1: все экземпляры Pocket задания (1 воркер или 10) получают
    /// ОДИНАКОВЫЕ параметры — engine_params не зависит от номера воркера,
    /// а фабрика закрывает над одним и тем же снимком opts.
    #[test]
    fn engine_params_identical_for_every_worker() {
        let o = opts();
        let first = engine_params(&o);
        for _ in 0..10 {
            assert_eq!(engine_params(&o), first);
        }
        // Default-политики тоже проходят без искажения (None ≠ Fixed(0))
        let mut o = opts();
        o.eos_tail_short = EosTail::Default;
        o.eos_tail_long = EosTail::Fixed(0);
        let p = engine_params(&o);
        assert_eq!(p.eos_tail_short, EosTail::Default);
        assert_eq!(p.eos_tail_long, EosTail::Fixed(0));
        assert_ne!(p.eos_tail_short, p.eos_tail_long);
    }
}

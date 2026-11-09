//! CUDA-P2A: экспериментальные GPU-резидентные состояния main-сессии через
//! ONNX Runtime I/O Binding (`ort` 2.0.0-rc.11, `Session::create_binding` /
//! `Session::run_binding`).
//!
//! Зачем: обычный `Session::run` на каждый кадр копирует все `state_*` входы
//! CPU→GPU и все `out_state_*` выходы GPU→CPU. Для main-графа это 18 стейтов,
//! из них 12 × [1,1000,16,64] f32 ≈ 46.9 MiB в одну сторону — ~94 MiB PCIe на
//! кадр (см. docs/CUDA_PROFILE_P1.md). Здесь выход кадра k остаётся на устройстве
//! и связывается входом кадра k+1 без хост-копии:
//!
//! - два `IoBinding` (ping-pong): пока один набор буферов читается как входы,
//!   другой принимает выходы текущего кадра (запись в тот же буфер, который
//!   читается, небезопасна — ORT переиспользует bound-буферы между прогонами);
//! - состояния сопоставляются по именам `state_*` ↔ `out_state_*`, а не по
//!   позиции; типы и статические формы сверяются с метаданными сессии;
//! - `reset()` только помечает состояния нулевыми: на первом прогоне после
//!   сброса в GPU-буферы один раз копируются CPU-нули (не на каждый кадр);
//! - маленькие выходы (`conditioning`, `eos_logit`) привязаны к CPU-памяти —
//!   единственные покадровые D2H-передачи (~4.1 КиБ).
//!
//! Путь включается только диагностическим флагом `SHTORM_CUDA_IO_BINDING=1`
//! (`Device::Cuda`); ошибки инициализации/форм — явные, без тихого отката.
//! Семантика генерации не меняется: те же входы, те же веса, тот же порядок
//! прогонов — ожидается побитовый паритет с обычным путём.

use anyhow::{bail, Context, Result};
use ndarray::{Array, IxDyn};
use ort::io_binding::IoBinding;
use ort::memory::{AllocationDevice, AllocatorType, MemoryInfo, MemoryType};
use ort::session::Session;
use ort::tensor::TensorElementType;
use ort::value::{DynTensorValueType, DynValue, Tensor, ValueType};

/// Диагностический флаг CUDA-P2A. Читается при загрузке движка (один раз на
/// экземпляр); по умолчанию выключен — работает проверенный путь.
pub(crate) fn cuda_io_binding_enabled() -> bool {
    std::env::var("SHTORM_CUDA_IO_BINDING").is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("on"))
}

/// Пара «вход state_* → выход out_state_*» с ожидаемыми типом и формой входа.
#[derive(Debug, Clone)]
pub(crate) struct StatePair {
    pub input: String,
    pub output: String,
    pub ty: TensorElementType,
    /// Форма входа из метаданных сессии (main: все размерности статические, > 0).
    pub shape: Vec<usize>,
}

impl StatePair {
    fn zero_value(&self) -> Result<DynValue> {
        let n: usize = self.shape.iter().product();
        let sh = IxDyn(&self.shape);
        let value: DynValue = match self.ty {
            TensorElementType::Float32 => {
                Tensor::from_array(Array::from_shape_vec(sh, vec![0f32; n])?)?.into_dyn()
            }
            TensorElementType::Float16 => {
                Tensor::from_array(Array::from_shape_vec(sh, vec![half::f16::ZERO; n])?)?.into_dyn()
            }
            TensorElementType::Int64 => {
                Tensor::from_array(Array::from_shape_vec(sh, vec![0i64; n])?)?.into_dyn()
            }
            TensorElementType::Bool => {
                Tensor::from_array(Array::from_shape_vec(sh, vec![false; n])?)?.into_dyn()
            }
            other => bail!(
                "CUDA-P2A: state {} has unsupported type {other:?}",
                self.input
            ),
        };
        Ok(value)
    }

    /// Сверка выхода с метаданными входа: тип и форма (стейты main статические).
    fn validate_output(&self, value: &DynValue) -> Result<()> {
        let ValueType::Tensor { ty, shape, .. } = value.dtype() else {
            bail!("CUDA-P2A: output {} is not a tensor", self.output)
        };
        if *ty != self.ty {
            bail!(
                "CUDA-P2A: output {} type {ty:?} != input {} type {:?}",
                self.output,
                self.input,
                self.ty
            );
        }
        let dims: Vec<i64> = shape.iter().copied().collect();
        let expected: Vec<i64> = self.shape.iter().map(|&d| d as i64).collect();
        if dims != expected {
            bail!(
                "CUDA-P2A: output {} shape {dims:?} != input {} static shape {expected:?}",
                self.output,
                self.input
            );
        }
        Ok(())
    }
}

/// GPU-резидентные состояния одной stateful-сессии (main) с ping-pong буферами.
pub(crate) struct IoStates {
    pairs: Vec<StatePair>,
    /// Не-stateful выходы в порядке объявления в графе (conditioning, eos_logit).
    plain_outputs: Vec<String>,
    /// Два набора связанных буферов: выход кадра k связывается входом k+1.
    bindings: [IoBinding; 2],
    /// Индекс binding'а, принимающего выходы следующего прогона.
    cur: usize,
    /// Самое новое состояние (выходы предыдущего прогона); пусто — нужны нули.
    states: Vec<DynValue>,
    /// Одноразовый диагностический вывод о размещении (первый прогон).
    reported: bool,
}

impl IoStates {
    /// Строит I/O Binding для сессии. `state_mem` — память для `out_state_*`
    /// (CUDA device в боевом пути), `cpu_mem` — CPU-память для маленьких
    /// выходов. Неподдерживаемые имена/типы/формы — явная ошибка.
    pub(crate) fn new(
        session: &Session,
        state_mem: &MemoryInfo,
        cpu_mem: &MemoryInfo,
    ) -> Result<Self> {
        let inputs: Vec<&str> = session.inputs().iter().map(|i| i.name()).collect();
        let outputs: Vec<&str> = session.outputs().iter().map(|o| o.name()).collect();

        let state_inputs: Vec<&str> = inputs
            .iter()
            .copied()
            .filter(|n| n.starts_with("state_"))
            .collect();
        if state_inputs.is_empty() {
            bail!("CUDA-P2A: session has no state_* inputs");
        }

        let mut pairs = Vec::with_capacity(state_inputs.len());
        for name in &state_inputs {
            let expected = format!("out_{name}");
            let meta = session
                .inputs()
                .iter()
                .find(|i| i.name() == *name)
                .with_context(|| format!("CUDA-P2A: input {name} disappeared"))?;
            let ValueType::Tensor { ty, shape, .. } = meta.dtype() else {
                bail!("CUDA-P2A: state input {name} is not a tensor")
            };
            let dims: Vec<i64> = shape.iter().copied().collect();
            if dims.iter().any(|&d| d < 0) {
                bail!(
                    "CUDA-P2A: state input {name} has dynamic shape {dims:?}; \
                     the GPU-resident path requires static state shapes"
                );
            }
            if !outputs.contains(&expected.as_str()) {
                bail!(
                    "CUDA-P2A: state input {name} has no matching output {expected} \
                     (matched by name, not position)"
                );
            }
            pairs.push(StatePair {
                input: (*name).to_string(),
                output: expected,
                ty: *ty,
                shape: dims.iter().map(|&d| d as usize).collect(),
            });
        }

        // Каждый out_state_* обязан соответствовать входу state_* — иначе граф
        // изменился и сопоставление по именам неполно.
        for out in outputs.iter().filter(|n| n.starts_with("out_state_")) {
            let input = out.strip_prefix("out_").expect("prefix checked");
            if !inputs.contains(&input) {
                bail!("CUDA-P2A: output {out} has no matching input {input}");
            }
        }

        let plain_outputs: Vec<String> = outputs
            .iter()
            .filter(|n| !n.starts_with("out_state_"))
            .map(|n| (*n).to_string())
            .collect();
        if plain_outputs.is_empty() {
            bail!("CUDA-P2A: session has no non-state outputs");
        }

        let b0 = session
            .create_binding()
            .context("CUDA-P2A: create_binding #0")?;
        let b1 = session
            .create_binding()
            .context("CUDA-P2A: create_binding #1")?;
        let mut bindings = [b0, b1];
        for b in &mut bindings {
            for p in &pairs {
                b.bind_output_to_device(p.output.as_str(), state_mem)
                    .with_context(|| format!("CUDA-P2A: bind output {}", p.output))?;
            }
            for name in &plain_outputs {
                b.bind_output_to_device(name.as_str(), cpu_mem)
                    .with_context(|| format!("CUDA-P2A: bind output {name}"))?;
            }
        }

        Ok(Self {
            pairs,
            plain_outputs,
            bindings,
            cur: 0,
            states: Vec::new(),
            reported: false,
        })
    }

    /// Сброс перед чанком/повтором anti-repeat: состояния станут нулевыми при
    /// следующем `run` (одна H2D-копия нулей на прогон, не на кадр).
    pub(crate) fn reset(&mut self) {
        self.states.clear();
    }

    /// Суммарный размер состояний в одну сторону (для диагностики VRAM/PCIe).
    pub(crate) fn state_bytes(&self) -> usize {
        self.pairs
            .iter()
            .map(|p| {
                let n: usize = p.shape.iter().product();
                n * match p.ty {
                    TensorElementType::Float32 => 4,
                    TensorElementType::Float16 => 2,
                    TensorElementType::Int64 => 8,
                    TensorElementType::Bool => 1,
                    _ => 0,
                }
            })
            .sum()
    }

    /// Размещение последнего состояния (диагностика/тесты): устройство и байты.
    #[cfg(test)]
    pub(crate) fn state_placement(&self) -> Option<(AllocationDevice, usize)> {
        let alloc = value_allocation(self.states.first()?)?;
        Some((alloc, self.state_bytes()))
    }

    /// Один прогон сессии с резидентными состояниями. `inputs` — все
    /// не-stateful входы (для main: sequence/text_embeddings). Возвращает
    /// не-stateful выходы в порядке графа; новые состояния остаются на GPU.
    /// `tim` — накопитель (время run_binding, время извлечения/проверок).
    pub(crate) fn run(
        &mut self,
        session: &mut Session,
        inputs: Vec<(String, DynValue)>,
        tim: Option<(&mut u64, &mut u64)>,
    ) -> Result<Vec<Vec<f32>>> {
        // Строгая проверка состава входов: все прочие входы сессии должны быть
        // переданы, лишние/неизвестные — ошибка (никакого молчаливого пропуска).
        let expected: Vec<String> = session
            .inputs()
            .iter()
            .map(|i| i.name().to_string())
            .filter(|n| !n.starts_with("state_"))
            .collect();
        let provided: Vec<String> = inputs.iter().map(|(n, _)| n.clone()).collect();
        for name in &expected {
            if !provided.contains(name) {
                bail!("CUDA-P2A: missing non-state input {name}");
            }
        }
        for name in &provided {
            if !expected.contains(name) {
                bail!("CUDA-P2A: unknown non-state input {name}");
            }
        }
        if provided.len() != expected.len() {
            bail!(
                "CUDA-P2A: duplicate non-state inputs: provided {provided:?}, expected {expected:?}"
            );
        }

        let cur = self.cur;
        {
            let b = &mut self.bindings[cur];
            for (name, value) in inputs {
                b.bind_input(name.as_str(), &value)
                    .with_context(|| format!("CUDA-P2A: bind input {name}"))?;
            }
            if self.states.is_empty() {
                for pair in &self.pairs {
                    let zero = pair.zero_value()?;
                    b.bind_input(pair.input.as_str(), &zero)
                        .with_context(|| format!("CUDA-P2A: bind zero state {}", pair.input))?;
                }
            } else {
                if self.states.len() != self.pairs.len() {
                    bail!(
                        "CUDA-P2A: state count {} != {}",
                        self.states.len(),
                        self.pairs.len()
                    );
                }
                for (pair, value) in self.pairs.iter().zip(&self.states) {
                    b.bind_input(pair.input.as_str(), value)
                        .with_context(|| format!("CUDA-P2A: bind state {}", pair.input))?;
                }
            }
        }

        let t_run = std::time::Instant::now();
        let mut outputs = session
            .run_binding(&self.bindings[cur])
            .context("CUDA-P2A: run_binding")?;
        // CPU-выходы и последующее использование состояний на других потоках
        // требуют завершения асинхронных передач провайдера (C API:
        // SynchronizeBoundOutputs).
        self.bindings[cur]
            .synchronize_outputs()
            .context("CUDA-P2A: synchronize_outputs")?;
        let run_elapsed = t_run.elapsed();

        let t_ext = std::time::Instant::now();
        let mut new_states = Vec::with_capacity(self.pairs.len());
        for pair in &self.pairs {
            let value = outputs
                .remove(pair.output.as_str())
                .with_context(|| format!("CUDA-P2A: no bound output {}", pair.output))?;
            pair.validate_output(&value)?;
            new_states.push(value);
        }
        let mut plain = Vec::with_capacity(self.plain_outputs.len());
        for name in &self.plain_outputs {
            let value = outputs
                .remove(name.as_str())
                .with_context(|| format!("CUDA-P2A: no bound output {name}"))?;
            plain.push(value.try_extract_tensor::<f32>()?.1.to_vec());
        }
        let ext_elapsed = t_ext.elapsed();

        if !self.reported {
            self.reported = true;
            let (alloc, bytes) = new_states
                .first()
                .and_then(|v| value_allocation(v).map(|a| (a, self.state_bytes())))
                .expect("states are non-empty tensors");
            log::info!(
                "CUDA-P2A: io binding active; state[0] on {alloc:?}, states {:.1} MiB one-way, plain outputs {:?} on CPU",
                bytes as f64 / (1024.0 * 1024.0),
                self.plain_outputs
            );
        }

        self.states = new_states;
        self.cur ^= 1;
        drop(outputs);
        if let Some((run_ns, ext_ns)) = tim {
            *run_ns += run_elapsed.as_nanos() as u64;
            *ext_ns += ext_elapsed.as_nanos() as u64;
        }
        Ok(plain)
    }
}

/// Устройство, на котором ORT разместил тензор (None для не-тензоров).
fn value_allocation(value: &DynValue) -> Option<AllocationDevice> {
    value
        .downcast_ref::<DynTensorValueType>()
        .ok()
        .map(|t| t.memory_info().allocation_device())
}

#[cfg(test)]
impl IoStates {
    /// Снимок состояний для тестов: f32-состояния как биты, i64 — как есть.
    /// Работает только на CPU-резидентных состояниях (для CUDA-пути тесты
    /// проверяют размещение, а не содержимое).
    pub(crate) fn snapshot(&self) -> Result<(Vec<Vec<u32>>, Vec<i64>)> {
        let mut f32_states = Vec::new();
        let mut i64_states = Vec::new();
        for v in &self.states {
            match v.dtype() {
                ValueType::Tensor {
                    ty: TensorElementType::Float32,
                    ..
                } => {
                    f32_states.push(
                        v.try_extract_tensor::<f32>()?
                            .1
                            .iter()
                            .map(|x| x.to_bits())
                            .collect(),
                    );
                }
                ValueType::Tensor {
                    ty: TensorElementType::Int64,
                    ..
                } => i64_states.extend_from_slice(v.try_extract_tensor::<i64>()?.1),
                other => bail!("snapshot: unexpected state dtype {other:?}"),
            }
        }
        Ok((f32_states, i64_states))
    }
}

/// CPU-memory info для маленьких выходов (conditioning/eos_logit).
pub(crate) fn cpu_output_memory() -> Result<MemoryInfo> {
    Ok(MemoryInfo::new(
        AllocationDevice::CPU,
        0,
        AllocatorType::Device,
        MemoryType::Default,
    )?)
}

/// CUDA device-memory info для `out_state_*` (обычная память устройства, не pinned).
pub(crate) fn cuda_state_memory(device_id: i32) -> Result<MemoryInfo> {
    Ok(MemoryInfo::new(
        AllocationDevice::CUDA,
        device_id,
        AllocatorType::Device,
        MemoryType::Default,
    )?)
}

// ---- тесты CUDA-P2A -------------------------------------------------------------------------
//
// Все тесты с ONNX — `#[ignore]` (как принято в проекте: требуют ORT_DYLIB_PATH и
// файлов моделей). Синтетические графы: crates/shtorm-engine/tests/models
// (пересоздание: tools/gen_p2a_models.py).
//
//   ORT_DYLIB_PATH=… cargo test -p shtorm-engine --lib io_states -- --ignored --nocapture
//   SHTORM_P2A_DEVICE=cuda ORT_DYLIB_PATH=… PATH=…deps/portable… \
//     cargo test -p shtorm-engine --lib io_states -- --ignored --nocapture
//
// Тесты пропускаются, если каталог синтетических моделей отсутствует.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pocket::{session_builder, Device};
    use ndarray::Array;
    use ort::session::builder::GraphOptimizationLevel;
    use ort::session::Session;
    use ort::value::Tensor;
    use std::path::{Path, PathBuf};

    fn models_dir() -> Option<PathBuf> {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/models");
        if repo.join("stateful.onnx").exists() {
            return Some(repo);
        }
        if let Ok(d) = std::env::var("SHTORM_P2A_MODELS") {
            let p = PathBuf::from(d);
            return p.join("stateful.onnx").exists().then_some(p);
        }
        None
    }

    fn use_cuda() -> bool {
        std::env::var("SHTORM_P2A_DEVICE").is_ok_and(|v| v.eq_ignore_ascii_case("cuda"))
    }

    fn make_session(path: &Path, cuda: bool) -> Result<Session> {
        let device = if cuda { Device::cuda(0) } else { Device::Cpu };
        let builder = session_builder(GraphOptimizationLevel::Level3, &device, 2)?;
        Ok(builder.commit_from_file(path)?)
    }

    fn f32_tensor(shape: Vec<usize>, data: Vec<f32>) -> Result<DynValue> {
        Ok(Tensor::from_array(Array::from_shape_vec(IxDyn(&shape), data)?)?.into_dyn())
    }

    /// Кадры: префиксный прогон с пустой последовательностью (как текст/голос
    /// в реальном generate) + обычные кадры.
    fn frames() -> Vec<Vec<f32>> {
        vec![
            Vec::new(),
            vec![0.1, 0.2, 0.3, 0.4],
            vec![0.5, -0.1, 0.25, 1.0],
            vec![-0.3, 0.7, 0.0, 0.2],
            vec![0.9, -0.9, 0.4, -0.4],
            vec![0.05, 0.15, -0.25, 0.35],
        ]
    }

    #[derive(Debug, PartialEq)]
    struct PlainOut {
        /// conditioning/eos_logit по кадрам (по 2 значения на кадр).
        values: Vec<Vec<f32>>,
        state_f32: Vec<Vec<u32>>,
        state_i64: Vec<i64>,
    }

    /// Эталон: обычный session.run с CPU-стейтами (как боевой путь до P2A).
    fn run_plain(session: &mut Session, frames: &[Vec<f32>]) -> Result<PlainOut> {
        let (mut s0, mut s1, mut s2) = (vec![0f32; 4], vec![0f32; 4], vec![0i64; 1]);
        let mut values = Vec::new();
        for seq in frames {
            let n = seq.len() / 4;
            let out = session.run(ort::inputs! {
                "sequence" => Tensor::from_array(Array::from_shape_vec((1, n, 4), seq.clone())?)?,
                "text_embeddings" => Tensor::from_array(Array::from_shape_vec((1, 0, 8), Vec::<f32>::new())?)?,
                "state_0" => Tensor::from_array(Array::from_shape_vec((1, 4), s0.clone())?)?,
                "state_1" => Tensor::from_array(Array::from_shape_vec((1, 4), s1.clone())?)?,
                "state_2" => Tensor::from_array(Array::from_shape_vec((1,), s2.clone())?)?,
            })?;
            s0 = out["out_state_0"].try_extract_tensor::<f32>()?.1.to_vec();
            s1 = out["out_state_1"].try_extract_tensor::<f32>()?.1.to_vec();
            s2 = out["out_state_2"].try_extract_tensor::<i64>()?.1.to_vec();
            values.push(out["conditioning"].try_extract_tensor::<f32>()?.1.to_vec());
            values.push(out["eos_logit"].try_extract_tensor::<f32>()?.1.to_vec());
        }
        Ok(PlainOut {
            values,
            state_f32: vec![
                s0.iter().map(|x| x.to_bits()).collect(),
                s1.iter().map(|x| x.to_bits()).collect(),
            ],
            state_i64: s2,
        })
    }

    /// Прогон экспериментального пути на заданных кадрах.
    fn run_binding(
        io: &mut IoStates,
        session: &mut Session,
        frames: &[Vec<f32>],
    ) -> Result<Vec<Vec<f32>>> {
        let mut values = Vec::new();
        for seq in frames {
            let n = seq.len() / 4;
            let out = io.run(
                session,
                vec![
                    (
                        "sequence".to_string(),
                        f32_tensor(vec![1, n, 4], seq.clone())?,
                    ),
                    (
                        "text_embeddings".to_string(),
                        f32_tensor(vec![1, 0, 8], Vec::new())?,
                    ),
                ],
                None,
            )?;
            values.extend(out);
        }
        Ok(values)
    }

    fn assert_bitwise(a: &[Vec<f32>], b: &[Vec<f32>]) {
        assert_eq!(a.len(), b.len(), "output count differs");
        for (i, (x, y)) in a.iter().zip(b).enumerate() {
            assert_eq!(x.len(), y.len(), "output {i} length differs");
            for (j, (u, v)) in x.iter().zip(y).enumerate() {
                assert_eq!(
                    u.to_bits(),
                    v.to_bits(),
                    "output {i}[{j}] differs: {u} vs {v}"
                );
            }
        }
    }

    /// Основной тест: ping-pong выход→вход побитово равен обычному пути,
    /// reset не оставляет загрязнения предыдущим состоянием.
    #[test]
    #[ignore]
    fn stateful_pingpong_matches_plain_and_reset() -> Result<()> {
        let Some(dir) = models_dir() else {
            return Ok(());
        };
        let cuda = use_cuda();
        let model = dir.join("stateful.onnx");
        let frames = frames();

        let mut plain_session = make_session(&model, cuda)?;
        let expected = run_plain(&mut plain_session, &frames)?;

        let mut bind_session = make_session(&model, cuda)?;
        let state_mem = if cuda {
            cuda_state_memory(0)?
        } else {
            cpu_output_memory()?
        };
        let cpu_mem = cpu_output_memory()?;
        let mut io = IoStates::new(&bind_session, &state_mem, &cpu_mem)?;
        assert_eq!(io.state_bytes(), 2 * 4 * 4 + 8); // два f32[4] + i64[1]

        // Первый проход: нулевые состояния + ping-pong.
        let got = run_binding(&mut io, &mut bind_session, &frames)?;
        assert_bitwise(&expected.values, &got);

        if !cuda {
            let (f32_states, i64_states) = io.snapshot()?;
            assert_eq!(f32_states, expected.state_f32, "final f32 states differ");
            assert_eq!(i64_states, expected.state_i64, "final i64 state differs");
        } else {
            let (alloc, bytes) = io.state_placement().expect("states present");
            assert_eq!(alloc, AllocationDevice::CUDA, "states must stay on GPU");
            assert_eq!(bytes, 2 * 4 * 4 + 8);
        }

        // Reset: следующий проход обязан дать те же значения (нет загрязнения).
        io.reset();
        let again = run_binding(&mut io, &mut bind_session, &frames)?;
        assert_bitwise(&expected.values, &again);

        // И ещё один reset внутри той же сессии — стабильность повторных генераций.
        io.reset();
        let third = run_binding(&mut io, &mut bind_session, &frames)?;
        assert_bitwise(&expected.values, &third);
        Ok(())
    }

    /// Имена состояний сопоставляются по факту (out_ + state_*); отсутствие
    /// пары и динамическая форма состояния — явные ошибки инициализации.
    #[test]
    #[ignore]
    fn rejects_unmapped_and_dynamic_states() -> Result<()> {
        let Some(dir) = models_dir() else {
            return Ok(());
        };
        let cpu_mem = cpu_output_memory()?;
        let state_mem = cpu_output_memory()?;

        let session = make_session(&dir.join("badnames.onnx"), false)?;
        let err = match IoStates::new(&session, &state_mem, &cpu_mem) {
            Ok(_) => panic!("badnames must be rejected"),
            Err(e) => e.to_string(),
        };
        assert!(
            err.contains("no matching output") && err.contains("out_state_0"),
            "unexpected error: {err}"
        );

        let session = make_session(&dir.join("dynamic.onnx"), false)?;
        let err = match IoStates::new(&session, &state_mem, &cpu_mem) {
            Ok(_) => panic!("dynamic state shape must be rejected"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("dynamic shape"), "unexpected error: {err}");
        Ok(())
    }

    /// Несовпадение типа выхода со входом обнаруживается на первом прогоне,
    /// а не замалчивается.
    #[test]
    #[ignore]
    fn rejects_output_type_mismatch() -> Result<()> {
        let Some(dir) = models_dir() else {
            return Ok(());
        };
        let mut session = make_session(&dir.join("badtype.onnx"), false)?;
        let state_mem = cpu_output_memory()?;
        let cpu_mem = cpu_output_memory()?;
        let mut io = IoStates::new(&session, &state_mem, &cpu_mem)?;
        let err = io
            .run(&mut session, Vec::new(), None)
            .expect_err("type mismatch must be reported")
            .to_string();
        assert!(err.contains("type"), "unexpected error: {err}");
        Ok(())
    }

    /// Создание/освобождение ресурсов: много циклов без падений и утечек
    /// (регресс на double-release OrtValue в GetBoundOutputValues).
    #[test]
    #[ignore]
    fn repeated_construct_and_drop_is_stable() -> Result<()> {
        let Some(dir) = models_dir() else {
            return Ok(());
        };
        let model = dir.join("stateful.onnx");
        for _ in 0..50 {
            let mut session = make_session(&model, false)?;
            let mut io = IoStates::new(&session, &cpu_output_memory()?, &cpu_output_memory()?)?;
            for seq in frames() {
                let n = seq.len() / 4;
                io.run(
                    &mut session,
                    vec![
                        ("sequence".to_string(), f32_tensor(vec![1, n, 4], seq)?),
                        (
                            "text_embeddings".to_string(),
                            f32_tensor(vec![1, 0, 8], Vec::new())?,
                        ),
                    ],
                    None,
                )?;
            }
        }
        Ok(())
    }
    /// Контролируемый замер стоимости I/O: синтетический граф с теми же 18
    /// состояниями, что у main (12 × [1,1000,16,64] f32 + 6 × [1] i64).
    /// Старая схема (States::run-семантика: CPU-OrtValue-стейты) гоняет
    /// ~93.8 MiB/прогон CPU↔GPU; новая — только Add на устройстве.
    /// Печатает мс/прогон и отношение; проверяет побитовое равенство выходов.
    #[test]
    #[ignore]
    fn bigstate_copy_cost_measured() -> Result<()> {
        if !use_cuda() {
            return Ok(());
        }
        let Some(dir) = models_dir() else {
            return Ok(());
        };
        let model = dir.join("bigstate.onnx");
        let runs = 20;

        let mut session = make_session(&model, true)?;
        let seq = vec![0.125f32; 32];
        let n_f32 = 1000 * 16 * 64;

        // Старая схема: состояния — CPU OrtValue, выход кадра -> вход следующего
        // (как States::run: без to_vec для больших стейтов, но с копиями ORT).
        let mut states: Vec<DynValue> = (0..12)
            .map(|_| f32_tensor(vec![1, 1000, 16, 64], vec![0f32; n_f32]))
            .chain((0..6).map(|_| f32_tensor_i64(vec![1], vec![0i64; 1])))
            .collect::<Result<Vec<_>>>()?;
        let out_names: Vec<String> = session
            .outputs()
            .iter()
            .map(|o| o.name().to_string())
            .collect();
        let state_out_names: Vec<String> = out_names
            .iter()
            .filter(|n| n.starts_with("out_state_"))
            .cloned()
            .collect();
        for _ in 0..3 {
            plain_step(&mut session, &seq, &mut states, &state_out_names)?;
        }
        let t_plain = std::time::Instant::now();
        let mut plain_cond = Vec::new();
        for _ in 0..runs {
            plain_cond = plain_step(&mut session, &seq, &mut states, &state_out_names)?;
        }
        let plain_ms = t_plain.elapsed().as_secs_f64() * 1000.0 / runs as f64;

        // Новая схема: те же состояния живут на GPU (ping-pong).
        let mut io = IoStates::new(&session, &cuda_state_memory(0)?, &cpu_output_memory()?)?;
        // Прогрев (первые прогоны включают нулевую инициализацию и аллокацию).
        for _ in 0..3 {
            io.run(
                &mut session,
                vec![(
                    "sequence".to_string(),
                    f32_tensor(vec![1, 1, 32], seq.clone())?,
                )],
                None,
            )?;
        }
        let mut io_cond = Vec::new();
        let t_io = std::time::Instant::now();
        for _ in 0..runs {
            let out = io.run(
                &mut session,
                vec![(
                    "sequence".to_string(),
                    f32_tensor(vec![1, 1, 32], seq.clone())?,
                )],
                None,
            )?;
            io_cond = out[0].clone();
        }
        let io_ms = t_io.elapsed().as_secs_f64() * 1000.0 / runs as f64;

        eprintln!(
            "bigstate: plain {plain_ms:.2} ms/run, io_binding {io_ms:.2} ms/run, \
             ratio {:.1}x; states {:.1} MiB one-way, copy roundtrip {:.1} MiB/run",
            plain_ms / io_ms,
            io.state_bytes() as f64 / (1024.0 * 1024.0),
            2.0 * io.state_bytes() as f64 / (1024.0 * 1024.0)
        );
        assert_eq!(
            plain_cond.iter().map(|x| x.to_bits()).collect::<Vec<_>>(),
            io_cond.iter().map(|x| x.to_bits()).collect::<Vec<_>>(),
            "conditioning differs between plain and io_binding paths"
        );
        assert!(
            io_ms < plain_ms / 2.0,
            "I/O binding must remove most of the copy cost (plain {plain_ms:.2}, io {io_ms:.2})"
        );
        Ok(())
    }

    /// Один прогон старой схемы: CPU-OrtValue-стейты -> feed -> out_state_*.
    fn plain_step(
        session: &mut Session,
        seq: &[f32],
        states: &mut Vec<DynValue>,
        state_out_names: &[String],
    ) -> Result<Vec<f32>> {
        let mut feed: Vec<(std::borrow::Cow<str>, ort::session::SessionInputValue)> = vec![(
            std::borrow::Cow::Borrowed("sequence"),
            f32_tensor(vec![1, 1, 32], seq.to_vec())?.into(),
        )];
        for (i, v) in states.drain(..).enumerate() {
            feed.push((std::borrow::Cow::Owned(format!("state_{i}")), v.into()));
        }
        let mut out = session.run(feed)?;
        let mut next = Vec::with_capacity(state_out_names.len());
        for name in state_out_names {
            next.push(out.remove(name.as_str()).expect("state output"));
        }
        *states = next;
        Ok(out["conditioning"].try_extract_tensor::<f32>()?.1.to_vec())
    }

    fn f32_tensor_i64(shape: Vec<usize>, data: Vec<i64>) -> Result<DynValue> {
        Ok(Tensor::from_array(Array::from_shape_vec(IxDyn(&shape), data)?)?.into_dyn())
    }
}

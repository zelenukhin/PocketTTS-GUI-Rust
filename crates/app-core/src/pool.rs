//! A4: пул параллельных воркеров синтеза.
//!
//! Свойства (по требованиям этапа):
//! - N воркеров, каждый владеет СВОИМ экземпляром движка (создаётся фабрикой
//!   в потоке воркера; никакой разделённой мутабельности ONNX-сессий/KV).
//! - Индекс чанка назначается ДО отправки в очередь (задача `Task`).
//! - Seed = base_seed + глобальный индекс чанка — не зависит от воркера.
//! - Результаты собираются строго в исходном порядке (буфер переупорядочивания).
//! - ОГРАНИЧЕННЫЕ очереди (sync_channel): задачи и результаты не копятся
//!   бесконечно; незавершённый ранний чанк создаёт backpressure — воркеры
//!   блокируются на отправке результата, не забирая новые задачи
//!   (max_buffered в JobSummary ограничен ёмкостью очереди результатов).
//! - Ошибка любого чанка: задание останавливается, ошибка передаётся наружу.
//! - Отмена (AtomicBool): новые задачи не выдаются (фидер), работающие
//!   дорабатывают текущий чанк и выходят; зависших потоков нет (join всех,
//!   фидер использует send_timeout + проверку отмены).
//! - Воркеры живут в рамках задания: после завершения Pocket-инстансы
//!   дропаются в своих потоках — ресурсы освобождены до возврата.
//!
//! Интерфейс под A5: `sink(index, pcm, pause_s)` вызывается строго по порядку
//! по мере готовности — к нему подключается потоковый WAV/ffmpeg-writer без
//! переписывания пула.

use anyhow::Result;
use crossbeam_channel::{bounded, RecvTimeoutError, SendTimeoutError};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::pipeline::DenoiseReport;

/// Одна задача пула: всё нужное воркеру (кроме его движка).
#[derive(Debug, Clone)]
pub struct Task {
    /// Глобальный индекс чанка (seed = base_seed + index).
    pub index: u64,
    /// Текст модели (ударения + char_rules применены заранее, в фидере).
    pub model_text: String,
    /// Пауза вставки после чанка (None — последний чанк файла).
    pub pause_s: Option<f32>,
}

/// Готовый чанк: речь после DSP (+ пауза для склейки, статус очистки).
#[derive(Debug, Clone)]
pub struct ChunkAudio {
    pub speech: Vec<f32>,
    pub pause_s: Option<f32>,
    pub denoise: DenoiseChunkStatus,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DenoiseChunkStatus {
    Applied,
    Off,
    Missing,
    Failed(String),
}

/// Воркер: владеет движком и параметрами задания (одинаковыми для всех
/// воркеров одного задания). Трейт позволяет тестировать пул на моках
/// без ONNX.
pub trait ChunkWorker: Send {
    /// Синтез + DSP одного чанка (время исполнения замеряет пул).
    fn process(&mut self, task: &Task) -> Result<ChunkAudio>;
}

/// Конфигурация пула.
#[derive(Debug, Clone)]
pub struct PoolConfig {
    pub workers: usize,
    /// Ёмкость ОГРАНИЧЕННЫХ очередей задач и результатов (backpressure).
    pub queue_cap: usize,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            workers: 1,
            queue_cap: 4,
        }
    }
}

/// Временная статистика одного чанка.
#[derive(Debug, Clone)]
#[allow(dead_code)] // worker_id/denoise/samples: журнал A5 и будущий GUI
pub struct ChunkTiming {
    pub index: usize,
    pub worker_id: usize,
    pub ms: u128,
    pub denoise: DenoiseChunkStatus,
    pub samples: usize,
}

/// Итоги задания.
#[derive(Debug)]
pub struct JobSummary {
    pub workers: usize,
    /// Чанков выдано в sink (в порядке).
    pub chunks_done: usize,
    pub timings: Vec<ChunkTiming>,
    /// Максимум готовых-но-не-по-порядку чанков в буфере (<= ёмкости очереди
    /// результатов): подтверждение ограниченного backpressure.
    pub max_buffered: usize,
    /// Максимум PCM-байт (f32) в буфере переупорядочивания за задание:
    /// диагностика «аудио не копится в памяти».
    pub max_buffered_bytes: usize,
    /// Базовый seed задания (при Random фиксируется здесь — для повтора).
    pub base_seed: u64,
    pub denoise: DenoiseReport,
    /// Задание остановлено по отмене (без ошибок).
    pub cancelled: bool,
}

pub(crate) fn panic_text(p: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "неизвестная паника".into()
    }
}

enum WorkerMsg {
    Done {
        index: u64,
        worker_id: usize,
        audio: ChunkAudio,
        ms: u128,
    },
    Failed {
        index: Option<u64>,
        #[allow(dead_code)] // worker_id: для будущей диагностики (лог A5)
        worker_id: usize,
        error: String,
    },
}

/// Политика seed задания: фиксированный или случайный (фиксируется один раз
/// на задание и возвращается в JobSummary — результат повторяем).
#[derive(Debug, Clone, Copy)]
pub enum SeedPolicy {
    Fixed(u64),
    Random,
}

impl SeedPolicy {
    pub fn resolve(self) -> u64 {
        match self {
            SeedPolicy::Fixed(s) => s,
            SeedPolicy::Random => std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(42)
                .max(1),
        }
    }
}

/// Запустить задание: N воркеров (каждый создан `factory(worker_id,
/// base_seed)` в своём потоке; base_seed разрешён из SeedPolicy и передан
/// фабрике — все воркеры одного задания идентичны по параметрам), задачи
/// `tasks` (индексы уже назначены), результаты строго по
/// порядку передаются в `sink(index, pcm, pause_s)` — Err из sink
/// останавливает задание и возвращается наружу. `cancel` — совместный флаг
/// отмены (можно выставить из sink или снаружи).
///
/// Возврат: Ok(summary) — задание дошло до конца или чисто отменено
/// (summary.cancelled); Err — ошибка чанка/старта воркера/sink.
pub fn run_job<W, F>(
    cfg: &PoolConfig,
    tasks: Vec<Task>,
    factory: impl Fn(usize, u64) -> Result<W> + Send + Sync + 'static,
    mut sink: F,
    cancel: &AtomicBool,
    seed: SeedPolicy,
) -> Result<JobSummary>
where
    W: ChunkWorker + 'static,
    F: FnMut(usize, &[f32], Option<f32>) -> Result<()>,
{
    anyhow::ensure!(cfg.workers >= 1, "workers >= 1");
    anyhow::ensure!(!tasks.is_empty(), "задание пусто");
    // Индексы задач обязаны быть непрерывны и в порядке 0..N-1: порядок и
    // seed+index — инварианты конвейера, «дырки» запрещены.
    for (i, t) in tasks.iter().enumerate() {
        anyhow::ensure!(
            t.index == i as u64,
            "индекс задачи {} вместо {} (индексы должны быть непрерывны 0..{})",
            t.index,
            i,
            tasks.len() - 1
        );
    }
    let workers = cfg.workers.min(tasks.len());
    let cap = cfg.queue_cap.max(workers);
    let base_seed = seed.resolve();
    let total = tasks.len();

    let (task_tx, task_rx) = bounded::<Task>(cap);
    let (res_tx, res_rx) = bounded::<WorkerMsg>(cap);
    let stop = Arc::new(AtomicBool::new(cancel.load(Ordering::SeqCst)));

    // Скользящее окно выдачи (backpressure по задачам): фидер не выдаёт задачу,
    // если её индекс дальше emitted + window. Тогда готовые-не-по-порядку чанки
    // в буфере main ограничены окном, канал — cap, в работе — workers.
    // Нужный (next_emit) чанк всегда уже отправлен (фидер строго по порядку).
    let emitted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let window = cap + workers;

    // --- фидер: индексы уже назначены в Task; отмена останавливает выдачу ---
    let feeder = {
        let stop = Arc::clone(&stop);
        let emitted = Arc::clone(&emitted);
        std::thread::spawn(move || {
            for t in tasks {
                loop {
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                    // окно: не выпускать задачу дальше emitted + window
                    while t.index as usize >= emitted.load(Ordering::SeqCst) + window {
                        if stop.load(Ordering::SeqCst) {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    // send_timeout: backpressure при полной очереди без
                    // зависания после отмены
                    match task_tx.send_timeout(t.clone(), Duration::from_millis(100)) {
                        Ok(()) => break,
                        Err(SendTimeoutError::Timeout(_)) => continue,
                        Err(SendTimeoutError::Disconnected(_)) => return,
                    }
                }
            }
        })
    };

    // --- воркеры: свой экземпляр движка; выход по отмене/закрытию канала ---
    let factory = Arc::new(factory);
    let mut handles = Vec::with_capacity(workers);
    for wid in 0..workers {
        let rx = task_rx.clone();
        let tx = res_tx.clone();
        let stop = Arc::clone(&stop);
        let factory = Arc::clone(&factory);
        handles.push(std::thread::spawn(move || {
            // Паника фабрики — диагностируемая ошибка задания, не обрыв потока.
            let started =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| factory(wid, base_seed)));
            let mut worker = match started {
                Ok(Ok(w)) => w,
                Ok(Err(e)) => {
                    let _ = tx.send(WorkerMsg::Failed {
                        index: None,
                        worker_id: wid,
                        error: format!("старт воркера {wid}: {e:#}"),
                    });
                    return;
                }
                Err(p) => {
                    let _ = tx.send(WorkerMsg::Failed {
                        index: None,
                        worker_id: wid,
                        // p.as_ref(): прямой &p ансайзит Box и даёт TypeId Box —
                        // текст паники терялся (латентный дефект A4, найден G6.1)
                        error: format!("паника фабрики воркера {wid}: {}", panic_text(p.as_ref())),
                    });
                    return;
                }
            };
            loop {
                let task = match rx.recv_timeout(Duration::from_millis(200)) {
                    Ok(t) => t,
                    Err(RecvTimeoutError::Timeout) => {
                        if stop.load(Ordering::SeqCst) {
                            break;
                        }
                        continue;
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                };
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                let t0 = Instant::now();
                // Паника process() — диагностируемая ошибка чанка.
                let produced = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    worker.process(&task)
                }));
                match produced {
                    Ok(Ok(mut audio)) => {
                        let ms = t0.elapsed().as_millis();
                        // пауза — из задачи (единственный источник, посчитан
                        // по исходному чанку в фидерной фазе)
                        audio.pause_s = task.pause_s;
                        if tx
                            .send(WorkerMsg::Done {
                                index: task.index,
                                worker_id: wid,
                                audio,
                                ms,
                            })
                            .is_err()
                        {
                            break; // приёмник закрыт — выходим
                        }
                    }
                    Ok(Err(e)) => {
                        let _ = tx.send(WorkerMsg::Failed {
                            index: Some(task.index),
                            worker_id: wid,
                            error: format!("{e:#}"),
                        });
                        break;
                    }
                    Err(p) => {
                        let _ = tx.send(WorkerMsg::Failed {
                            index: Some(task.index),
                            worker_id: wid,
                            error: format!("паника воркера {wid}: {}", panic_text(p.as_ref())),
                        });
                        break;
                    }
                }
            }
            // Pocket (если RealWorker) дропнут здесь — в потоке воркера,
            // до возврата run_job (join ниже)
        }));
    }
    drop(res_tx); // канал закроется, когда выйдут все воркеры

    // --- сборка строго по порядку + отмена/ошибки/backpressure-метрика ---
    let mut next_emit = 0usize;
    let mut buffer: HashMap<u64, (usize, ChunkAudio, u128)> = HashMap::new();
    let mut timings: Vec<ChunkTiming> = Vec::with_capacity(total);
    let mut denoise = DenoiseReport::default();
    let mut first_error: Option<String> = None;
    let mut sink_error: Option<anyhow::Error> = None;
    let mut max_buffered = 0usize;
    let mut max_buffered_bytes = 0usize;
    let mut buffered_bytes = 0usize;
    let mut external_cancel = false;

    while next_emit < total {
        if cancel.load(Ordering::SeqCst) {
            external_cancel = true;
            stop.store(true, Ordering::SeqCst);
            break;
        }
        let msg = match res_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(m) => m,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                // все воркеры вышли раньше времени без отмены/ошибки —
                // это НЕ успех: канал закрылся преждевременно
                if first_error.is_none() && sink_error.is_none() && next_emit < total {
                    first_error = Some(format!(
                        "канал результатов закрылся преждевременно: готово {next_emit} из {total}"
                    ));
                }
                stop.store(true, Ordering::SeqCst);
                break;
            }
        };
        match msg {
            WorkerMsg::Done {
                index,
                worker_id,
                audio,
                ms,
            } => {
                timings.push(ChunkTiming {
                    index: index as usize,
                    worker_id,
                    ms,
                    denoise: audio.denoise.clone(),
                    samples: audio.speech.len(),
                });
                match &audio.denoise {
                    DenoiseChunkStatus::Applied => denoise.applied += 1,
                    DenoiseChunkStatus::Off => denoise.off += 1,
                    DenoiseChunkStatus::Missing => denoise.missing += 1,
                    DenoiseChunkStatus::Failed(e) => {
                        denoise.failed += 1;
                        if denoise.first_error.is_none() {
                            denoise.first_error = Some(e.clone());
                        }
                    }
                }
                buffered_bytes += audio.speech.len() * std::mem::size_of::<f32>();
                buffer.insert(index, (worker_id, audio, ms));
                max_buffered = max_buffered.max(buffer.len());
                max_buffered_bytes = max_buffered_bytes.max(buffered_bytes);
                while let Some((_wid, audio, _ms)) = buffer.remove(&(next_emit as u64)) {
                    if let Err(e) = sink(next_emit, &audio.speech, audio.pause_s) {
                        sink_error = Some(e);
                        break;
                    }
                    buffered_bytes = buffered_bytes
                        .saturating_sub(audio.speech.len() * std::mem::size_of::<f32>());
                    next_emit += 1;
                    emitted.store(next_emit, Ordering::SeqCst); // двигаем окно фидера
                }
            }
            WorkerMsg::Failed {
                index,
                worker_id: _,
                error,
            } => {
                if first_error.is_none() {
                    first_error = Some(match index {
                        Some(i) => format!("чанк {}: {error}", i + 1),
                        None => error,
                    });
                }
                stop.store(true, Ordering::SeqCst);
            }
        }
        if first_error.is_some() || sink_error.is_some() {
            stop.store(true, Ordering::SeqCst);
        }
    }

    // --- дренаж: дорабатываем/забираем остатки, пока живы воркеры ---
    loop {
        match res_rx.recv_timeout(Duration::from_millis(100)) {
            // Сбой СТАРТА воркера (index: None — фабрика/паника) не должен
            // теряться, если остальные воркеры успели доделать всё задание:
            // сообщение приходит позже выхода главного цикла (гонка, ловилась
            // флейком worker_start_failure_propagates). Ошибки отдельных
            // чанков в дренаже остаются игнорируемыми (задание уже
            // остановлено, первая ошибка уже зафиксирована).
            Ok(WorkerMsg::Failed {
                index: None, error, ..
            }) => {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
            Ok(_) => {} // готовые остатки не обрабатываем (задание уже остановлено)
            Err(RecvTimeoutError::Timeout) => {
                // холостой таймаут при долгом текущем чанке: ждём, пока все
                // воркеры закончат и закроют канал
                if handles.iter().all(|h| h.is_finished()) {
                    break;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    let mut thread_panics: Vec<String> = Vec::new();
    if let Err(p) = feeder.join() {
        thread_panics.push(format!("фидер паниковал: {}", panic_text(p.as_ref())));
    }
    for (wid, h) in handles.into_iter().enumerate() {
        if let Err(p) = h.join() {
            thread_panics.push(format!(
                "поток воркера {wid} паниковал: {}",
                panic_text(p.as_ref())
            ));
        }
    }

    if let Some(e) = first_error {
        return Err(anyhow::anyhow!("задание прервано ошибкой: {e}"));
    }
    if let Some(e) = sink_error {
        return Err(anyhow::anyhow!("задание прервано приёмником: {e:#}"));
    }
    if let Some(e) = thread_panics.first() {
        return Err(anyhow::anyhow!("задание прервано паникой потока: {e}"));
    }
    if !external_cancel && next_emit < total {
        return Err(anyhow::anyhow!(
            "задание завершилось неполным: готово {next_emit} из {total}"
        ));
    }
    Ok(JobSummary {
        workers,
        chunks_done: next_emit,
        timings,
        max_buffered,
        max_buffered_bytes,
        base_seed,
        denoise,
        cancelled: external_cancel,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Мок-воркер: маркер = index (детерминизм), настраиваемые задержки/ошибки.
    struct Mock {
        sleep_ms: HashMap<u64, u64>,
        fail_on: Option<u64>,
    }
    impl ChunkWorker for Mock {
        fn process(&mut self, task: &Task) -> Result<ChunkAudio> {
            if let Some(f) = self.fail_on {
                if task.index == f {
                    anyhow::bail!("синтетическая ошибка чанка {}", task.index);
                }
            }
            if let Some(ms) = self.sleep_ms.get(&task.index) {
                std::thread::sleep(Duration::from_millis(*ms));
            }
            Ok(ChunkAudio {
                speech: vec![task.index as f32; (task.index as usize % 3) + 1],
                pause_s: task.pause_s,
                denoise: DenoiseChunkStatus::Off,
            })
        }
    }

    fn tasks(n: u64) -> Vec<Task> {
        (0..n)
            .map(|i| Task {
                index: i,
                model_text: format!("текст{i}"),
                pause_s: if i + 1 == n { None } else { Some(0.1) },
            })
            .collect()
    }

    fn no_cancel() -> std::sync::atomic::AtomicBool {
        AtomicBool::new(false)
    }

    #[test]
    fn two_workers_strict_order_no_loss_no_duplication() {
        let cfg = PoolConfig {
            workers: 2,
            queue_cap: 2,
        };
        let mut got: Vec<(usize, Vec<f32>)> = Vec::new();
        let summary = run_job(
            &cfg,
            tasks(20),
            |_, _| {
                Ok(Mock {
                    sleep_ms: HashMap::from([(0, 120), (7, 90), (13, 60)]),
                    fail_on: None,
                })
            },
            |i, pcm, _p| {
                got.push((i, pcm.to_vec()));
                Ok(())
            },
            &no_cancel(),
            SeedPolicy::Fixed(42),
        )
        .unwrap();
        assert_eq!(got.len(), 20);
        for (k, (i, pcm)) in got.iter().enumerate() {
            assert_eq!(*i, k, "порядок строгий");
            assert_eq!(pcm.len(), (k % 3) + 1);
            assert!(pcm.iter().all(|&v| v == k as f32), "маркер чанка");
        }
        assert_eq!(summary.chunks_done, 20);
        assert!(!summary.cancelled);
        assert_eq!(summary.base_seed, 42);
        assert!(
            summary.max_buffered <= 2 * 2 + 2,
            "ограниченный буфер (окно+канал)"
        );
    }

    #[test]
    fn one_vs_two_workers_same_output() {
        let run = |workers| {
            let mut out = Vec::new();
            run_job(
                &PoolConfig {
                    workers,
                    queue_cap: 3,
                },
                tasks(10),
                |_, _| {
                    Ok(Mock {
                        sleep_ms: HashMap::new(),
                        fail_on: None,
                    })
                },
                |i, pcm, p| {
                    out.push((i, pcm.to_vec(), p));
                    Ok(())
                },
                &no_cancel(),
                SeedPolicy::Fixed(7),
            )
            .unwrap();
            out
        };
        assert_eq!(run(1), run(2), "1 и 2 воркера дают идентичный поток");
    }

    #[test]
    fn worker_error_stops_job_and_propagates() {
        let cfg = PoolConfig {
            workers: 2,
            queue_cap: 2,
        };
        let mut seen = 0usize;
        let err = run_job(
            &cfg,
            tasks(12),
            |_, _| {
                Ok(Mock {
                    sleep_ms: HashMap::new(),
                    fail_on: Some(3),
                })
            },
            |_, _, _| {
                seen += 1;
                Ok(())
            },
            &no_cancel(),
            SeedPolicy::Fixed(1),
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("чанк 4"), "сообщение об ошибке чанка: {msg}");
        assert!(seen < 12, "выдача остановлена (выдано {seen})");
    }

    #[test]
    fn cancel_stops_early_without_hang() {
        let cancel = AtomicBool::new(false);
        let cfg = PoolConfig {
            workers: 2,
            queue_cap: 2,
        };
        let c = &cancel;
        let summary = run_job(
            &cfg,
            tasks(200),
            |_, _| {
                Ok(Mock {
                    sleep_ms: (0..200).map(|i| (i, 5)).collect(),
                    fail_on: None,
                })
            },
            |i, _, _| {
                if i >= 3 {
                    c.store(true, Ordering::SeqCst); // «пользователь» отменил
                }
                Ok(())
            },
            &cancel,
            SeedPolicy::Fixed(5),
        )
        .unwrap();
        assert!(summary.cancelled);
        assert!(
            summary.chunks_done < 200,
            "выдано {} < 200",
            summary.chunks_done
        );
        assert!(summary.chunks_done >= 3);
    }

    #[test]
    fn slow_first_chunk_backpressure_bounded() {
        // ранний чанк долгий, остальные мгновенные: буфер готовых-не-по-порядку
        // не превосходит ёмкость очереди результатов
        let cfg = PoolConfig {
            workers: 2,
            queue_cap: 2,
        };
        let summary = run_job(
            &cfg,
            tasks(30),
            |_, _| {
                Ok(Mock {
                    sleep_ms: HashMap::from([(0, 300)]),
                    fail_on: None,
                })
            },
            |_, _, _| Ok(()),
            &no_cancel(),
            SeedPolicy::Fixed(9),
        )
        .unwrap();
        assert_eq!(summary.chunks_done, 30);
        // жёсткая граница памяти переупорядочивания: окно (cap+workers) +
        // ёмкость канала результатов; в худшем «ранний долгий» сценарии
        // реально наблюдается существенно меньше
        let bound = cfg.queue_cap * 2 + cfg.workers;
        assert!(
            summary.max_buffered <= bound,
            "max_buffered {} <= окно+канал ({bound})",
            summary.max_buffered
        );
        assert!(
            summary.max_buffered < 30,
            "реальный backpressure: {} << 30 (было бы 29 без окна)",
            summary.max_buffered
        );
    }

    #[test]
    fn pool_reused_for_second_job_workers_created_per_job() {
        let created = Arc::new(Mutex::new(0usize));
        let cfg = PoolConfig {
            workers: 2,
            queue_cap: 2,
        };
        for _ in 0..2 {
            let created = Arc::clone(&created);
            let out = Mutex::new(Vec::new());
            let summary = run_job(
                &cfg,
                tasks(6),
                move |_, _| {
                    let mut g = created.lock().unwrap();
                    *g += 1;
                    Ok(Mock {
                        sleep_ms: HashMap::new(),
                        fail_on: None,
                    })
                },
                |i, pcm, _| {
                    out.lock().unwrap().push((i, pcm.len()));
                    Ok(())
                },
                &no_cancel(),
                SeedPolicy::Fixed(2),
            )
            .unwrap();
            assert_eq!(summary.chunks_done, 6);
        }
        assert_eq!(*created.lock().unwrap(), 4, "2 задания × 2 воркера");
    }

    #[test]
    fn random_seed_resolved_once_and_reported() {
        let s1 = SeedPolicy::Random.resolve();
        let mut s2 = SeedPolicy::Random.resolve();
        assert!(s1 >= 1 && s2 >= 1);
        // На Windows таймер может давать одинаковые наносекунды подряд
        // (release-сборка успевает в один тик) — ждём реального изменения
        // времени; обычно достаточно 0–2 итераций.
        for _ in 0..10_000 {
            if s1 != s2 {
                break;
            }
            s2 = SeedPolicy::Random.resolve();
        }
        assert_ne!(s1, s2, "случайные seed различаются");
    }

    #[test]
    fn non_contiguous_indices_rejected() {
        let mut t = tasks(3);
        t[1].index = 5; // дырка в индексах
        let err = run_job(
            &PoolConfig {
                workers: 1,
                queue_cap: 2,
            },
            t,
            |_, _| {
                Ok(Mock {
                    sleep_ms: HashMap::new(),
                    fail_on: None,
                })
            },
            |_, _, _| Ok(()),
            &no_cancel(),
            SeedPolicy::Fixed(1),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("непрерывны"), "{err:#}");
    }

    /// Паника фабрики и process() превращается в ошибку задания.
    /// Тело выполняется ТОЛЬКО в дочернем процессе (env-маркер): дочерний
    /// процесс — тестовый бинарь, запущенный с фильтром по этому имени теста.
    /// Родительский тест ограничивает время и убивает зависшего ребёнка.
    #[test]
    fn panic_scenarios_child_entry() {
        if std::env::var("SHTORM_POOL_PANIC_CHILD").is_err() {
            return; // в обычном прогоне — no-op
        }
        // 1) паника в фабрике
        let e = run_job(
            &PoolConfig {
                workers: 2,
                queue_cap: 2,
            },
            tasks(4),
            |_, _| -> Result<PanicFactoryInner> { panic!("фабрика легла") },
            |_, _, _| Ok(()),
            &no_cancel(),
            SeedPolicy::Fixed(1),
        )
        .unwrap_err();
        assert!(format!("{e:#}").contains("паника фабрики"), "{e:#}");
        // 2) паника в process()
        struct PanicWorker;
        impl ChunkWorker for PanicWorker {
            fn process(&mut self, _t: &Task) -> Result<ChunkAudio> {
                panic!("process легла");
            }
        }
        let e = run_job(
            &PoolConfig {
                workers: 1,
                queue_cap: 2,
            },
            tasks(3),
            |_, _| Ok(PanicWorker),
            |_, _, _| Ok(()),
            &no_cancel(),
            SeedPolicy::Fixed(1),
        )
        .unwrap_err();
        assert!(format!("{e:#}").contains("паника воркера"), "{e:#}");
        println!("CHILD_OK");
    }

    /// Тип-заглушка для компиляции паникующей фабрики (не создаётся никогда).
    struct PanicFactoryInner;
    impl ChunkWorker for PanicFactoryInner {
        fn process(&mut self, _t: &Task) -> Result<ChunkAudio> {
            unreachable!()
        }
    }

    #[test]
    fn panic_scenarios_do_not_hang_parent() {
        if std::env::var("SHTORM_POOL_PANIC_CHILD").is_ok() {
            return; // это дочерний процесс — родительский тест сам решает
        }
        let exe = std::env::current_exe().expect("current_exe");
        let mut child = std::process::Command::new(exe)
            .args([
                "--exact",
                "pool::tests::panic_scenarios_child_entry",
                "--test-threads=1",
            ])
            .env("SHTORM_POOL_PANIC_CHILD", "1")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn child test");
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        loop {
            match child.try_wait().expect("try_wait") {
                Some(status) => {
                    assert!(status.success(), "дочерний тест паник упал: {status:?}");
                    break;
                }
                None => {
                    if std::time::Instant::now() > deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        panic!("дочерний тест паник завис — убит по таймауту");
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }

    #[test]
    fn worker_start_failure_propagates() {
        let cfg = PoolConfig {
            workers: 2,
            queue_cap: 2,
        };
        let err = run_job(
            &cfg,
            tasks(4),
            |wid, _| {
                if wid == 0 {
                    anyhow::bail!("нет модели")
                }
                Ok(Mock {
                    sleep_ms: HashMap::new(),
                    fail_on: None,
                })
            },
            |_, _, _| Ok(()),
            &no_cancel(),
            SeedPolicy::Fixed(3),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("старт воркера"));
    }
}

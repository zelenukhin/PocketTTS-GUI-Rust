//! G6.1: постоянный пул воркеров для пакетной обработки файлов.
//!
//! Модель (по результатам CUDA-P2B.2/P2B.3):
//! - N воркеров, каждый владеет СВОИМ `Pocket`/ONNX-сессиями, создан один раз
//!   до первого файла и живёт до конца пакета;
//! - один файл целиком обрабатывается одним воркером (чанки не распределяются
//!   между воркерами) — `run_job` с `workers = 1`;
//! - общая динамическая очередь файлов: освободившийся воркер немедленно
//!   берёт следующий (переход файл→файл ≈ 0–2 мс по замерам);
//! - Pocket освобождается только после завершения всего пакета (или при
//!   отравлении слота — см. ниже).
//!
//! Переиспользование воркера между файлами безопасно для результата:
//! `Pocket::say_chunk` пересеивает RNG как `seed + chunk_index` на КАЖДЫЙ
//! вызов, поэтому достаточно установить per-file seed перед первым чанком
//! (`RealWorker::set_seed` через `prepare` фабрики) — поток идентичен свежему
//! Pocket с этим seed (побитовый паритет доказан P2B.2 §11, включая повтор
//! файла на прогретом Pocket).
//!
//! Отравление (poisoning): если файл упал с ошибкой движка или воркер
//! запаниковал, Pocket может остаться в неопределённом состоянии — такой слот
//! опустошается и НЕ переиспользуется (ошибки публикации и чтения текста
//! движком не являются и слот не трогают — классификация на стороне
//! вызывающего, см. gui/job.rs G6.1).

use anyhow::Result;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::pool::{panic_text, ChunkWorker};
use crate::worker::RealWorker;

/// Слот постоянного воркера. `Some(w)` — воркер свободен и может быть
/// «одолжен» на файл; `None` — занят файлом, отравлен или освобождён.
pub type WorkerSlot<W> = Arc<Mutex<Option<W>>>;

/// Воркер, одолженный у постоянного пула на время одного файла: `run_job`
/// создаёт и дропает экземпляры сам, а Pocket обязан пережить файл — поэтому
/// `Drop` возвращает воркер в слот. Если слот за это время отравлен
/// (занят чем-то другим), возвращённый воркер просто дропается.
pub struct Loaned<W> {
    slot: WorkerSlot<W>,
    inner: Option<W>,
}

impl<W: ChunkWorker> ChunkWorker for Loaned<W> {
    fn process(&mut self, task: &crate::pool::Task) -> Result<crate::pool::ChunkAudio> {
        self.inner
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("одолженный воркер пуст"))?
            .process(task)
    }
}

impl<W> Drop for Loaned<W> {
    fn drop(&mut self) {
        if let Some(w) = self.inner.take() {
            if let Ok(mut slot) = self.slot.lock() {
                *slot = Some(w);
            }
        }
    }
}

/// Фабрика одалживания: каждый вызов берёт воркер из слота, прогоняет его
/// через `prepare(worker, seed)` (per-file seed постоянного Pocket) и
/// возвращает `Loaned`. `seed` — base_seed, разрешённый КОНВЕЙЕРОМ файла
/// (`synthesize_stream_with_worker_factory`): фиксированный из настроек или
/// случайный — на каждый файл свой, как в пути «модель на файл».
pub fn loaned_worker_factory<W, P>(
    slot: WorkerSlot<W>,
    prepare: P,
) -> impl Fn(usize, u64) -> Result<Loaned<W>> + Send + Sync + use<W, P>
where
    W: ChunkWorker,
    P: Fn(&mut W, u64) + Send + Sync,
{
    move |_worker_id: usize, seed: u64| {
        let mut worker = slot
            .lock()
            .map_err(|_| anyhow::anyhow!("мьютекс воркера отравлен"))?
            .take()
            .ok_or_else(|| {
                anyhow::anyhow!("постоянный воркер занят или отравлен — файл не запускается")
            })?;
        prepare(&mut worker, seed);
        Ok(Loaned {
            slot: Arc::clone(&slot),
            inner: Some(worker),
        })
    }
}

/// Фабрика одалживания для `RealWorker`: устанавливает per-file seed на
/// постоянном Pocket (эквивалентно свежей загрузке с этим seed).
pub fn loaned_real_worker_factory(
    slot: WorkerSlot<RealWorker>,
) -> impl Fn(usize, u64) -> Result<Loaned<RealWorker>> + Send + Sync + use<> {
    loaned_worker_factory(slot, |w, seed| w.set_seed(seed))
}

/// Итог параллельной загрузки пула.
pub struct SpawnedWorkers<W> {
    pub slots: Vec<WorkerSlot<W>>,
    /// Число успешно загруженных воркеров (== slots.len()).
    pub loaded: usize,
    /// Время загрузки каждого успешного воркера (мс; параллельная загрузка).
    pub load_ms: Vec<u128>,
    /// Сбои загрузки: (worker_id, сообщение). Паника фабрики — диагностируемая
    /// ошибка ЭТОГО воркера; повторов загрузки нет (политика OOM G6.1).
    pub failures: Vec<(usize, String)>,
}

/// Загрузить `count` постоянных воркеров параллельно (каждый в своём потоке —
/// по замерам P2B.2 wall времени загрузки ≈ одной загрузке). Фабрика та же,
/// что использует `run_job` (`real_worker_factory`); Pocket создаётся здесь
/// ровно один раз на пакет.
pub fn spawn_workers<W, F>(count: usize, factory: F, base_seed: u64) -> SpawnedWorkers<W>
where
    F: Fn(usize, u64) -> Result<W> + Send + Sync + 'static,
    W: Send + 'static,
{
    let factory = Arc::new(factory);
    let mut handles = Vec::with_capacity(count);
    for wid in 0..count {
        let factory = Arc::clone(&factory);
        handles.push(std::thread::spawn(move || {
            let t0 = Instant::now();
            let started =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| factory(wid, base_seed)));
            match started {
                Ok(Ok(w)) => Ok((Arc::new(Mutex::new(Some(w))), t0.elapsed().as_millis())),
                Ok(Err(e)) => Err(format!("{e:#}")),
                Err(p) => Err(format!(
                    "паника загрузки воркера {wid}: {}",
                    // p.as_ref(): &Box<dyn Any> ансайзится в &dyn Any с TypeId
                    // самого Box (не payload) — текст паники терялся бы
                    panic_text(p.as_ref())
                )),
            }
        }));
    }
    let mut out = SpawnedWorkers {
        slots: Vec::new(),
        loaded: 0,
        load_ms: Vec::new(),
        failures: Vec::new(),
    };
    for (wid, h) in handles.into_iter().enumerate() {
        match h.join() {
            Ok(Ok((slot, ms))) => {
                out.slots.push(slot);
                out.loaded += 1;
                out.load_ms.push(ms);
            }
            Ok(Err(e)) => out.failures.push((wid, e)),
            Err(p) => out.failures.push((
                wid,
                format!("паника потока загрузки: {}", panic_text(p.as_ref())),
            )),
        }
    }
    out
}

/// Итог динамической очереди файлов.
pub struct DynamicQueueOutcome<R> {
    /// Результаты по ПОЗИЦИЯМ файлов (0..total) в исходном порядке;
    /// `None` — файл не был запущен (отмена до выдачи или гибель воркеров).
    pub results: Vec<Option<R>>,
    /// Фатальные сбои воркеров: (worker_id, позиция файла, сообщение паники).
    pub worker_fatal: Vec<(usize, usize, String)>,
    /// Какие позиции файлов забрал каждый воркер (диагностика распределения:
    /// нет повторных назначений и потерь).
    pub claimed: Vec<Vec<usize>>,
}

/// Динамическая очередь: каждый воркер в собственном потоке берёт следующую
/// позицию из общего счётчика и выполняет `job(file_pos, worker_id, slot)`.
/// Отмена останавливает ВЫДАЧУ новых файлов (текущие дорабатывают — отмена
/// внутри файла — забота `job`/конвейера). Паника `job` отравляет слот
/// (Pocket не переиспользуется) и останавливает поток этого воркера;
/// остальные воркеры разбирают очередь дальше. Перед выходом поток
/// освобождает свой Pocket (в потоке воркера, как в pool.rs).
pub fn run_dynamic_queue<W, R, F>(
    slots: Vec<WorkerSlot<W>>,
    total: usize,
    cancel: &Arc<AtomicBool>,
    job: F,
) -> DynamicQueueOutcome<R>
where
    W: Send + 'static,
    F: Fn(usize, usize, &WorkerSlot<W>) -> R + Send + Sync + 'static,
    R: Send + 'static,
{
    let next = Arc::new(AtomicUsize::new(0));
    let results: Arc<Mutex<Vec<Option<R>>>> =
        Arc::new(Mutex::new((0..total).map(|_| None).collect()));
    let fatal: Arc<Mutex<Vec<(usize, usize, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let claimed: Arc<Mutex<Vec<Vec<usize>>>> =
        Arc::new(Mutex::new(slots.iter().map(|_| Vec::new()).collect()));
    let job = Arc::new(job);

    let mut handles = Vec::with_capacity(slots.len());
    for (wid, slot) in slots.into_iter().enumerate() {
        let next = Arc::clone(&next);
        let results = Arc::clone(&results);
        let fatal = Arc::clone(&fatal);
        let claimed = Arc::clone(&claimed);
        let job = Arc::clone(&job);
        let cancel = Arc::clone(cancel);
        handles.push(std::thread::spawn(move || {
            loop {
                if cancel.load(Ordering::SeqCst) {
                    break;
                }
                let i = next.fetch_add(1, Ordering::SeqCst);
                if i >= total {
                    break;
                }
                if let Ok(mut c) = claimed.lock() {
                    if wid < c.len() {
                        c[wid].push(i);
                    }
                }
                let produced =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(i, wid, &slot)));
                match produced {
                    Ok(r) => {
                        if let Ok(mut res) = results.lock() {
                            if i < res.len() {
                                res[i] = Some(r);
                            }
                        }
                    }
                    Err(p) => {
                        // Pocket в неопределённом состоянии — отравить слот
                        if let Ok(mut s) = slot.lock() {
                            *s = None;
                        }
                        if let Ok(mut f) = fatal.lock() {
                            f.push((wid, i, panic_text(p.as_ref())));
                        }
                        break;
                    }
                }
            }
            // Pocket освобождается здесь — в потоке воркера, до выхода потока
            if let Ok(mut s) = slot.lock() {
                *s = None;
            }
        }));
    }

    let mut thread_panics = Vec::new();
    for (wid, h) in handles.into_iter().enumerate() {
        if let Err(p) = h.join() {
            thread_panics.push((wid, usize::MAX, panic_text(p.as_ref())));
        }
    }

    let results = Arc::try_unwrap(results)
        .map(|m| m.into_inner().unwrap_or_default())
        .unwrap_or_default();
    let mut fatal = Arc::try_unwrap(fatal)
        .map(|m| m.into_inner().unwrap_or_default())
        .unwrap_or_default();
    fatal.extend(thread_panics);
    let claimed = Arc::try_unwrap(claimed)
        .map(|m| m.into_inner().unwrap_or_default())
        .unwrap_or_default();

    DynamicQueueOutcome {
        results,
        worker_fatal: fatal,
        claimed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::{run_job, ChunkAudio, PoolConfig, SeedPolicy, Task};

    /// Мок-воркер с записью установленного seed.
    struct Mock {
        id: u32,
        seed_set: Arc<Mutex<Vec<u64>>>,
    }
    impl ChunkWorker for Mock {
        fn process(&mut self, _task: &Task) -> Result<ChunkAudio> {
            Ok(ChunkAudio {
                speech: vec![self.id as f32],
                pause_s: None,
                denoise: crate::pool::DenoiseChunkStatus::Off,
            })
        }
    }

    fn tasks(n: u64) -> Vec<Task> {
        (0..n)
            .map(|i| Task {
                index: i,
                model_text: format!("т{i}"),
                pause_s: None,
            })
            .collect()
    }

    /// Основной сценарий фабрики: loan → prepare(seed) → Drop → возврат;
    /// параллельный файл на том же слоте — явная ошибка «занят».
    #[test]
    fn loaned_worker_lifecycle() {
        let seeds = Arc::new(Mutex::new(Vec::new()));
        let slot: WorkerSlot<Mock> = Arc::new(Mutex::new(Some(Mock {
            id: 1,
            seed_set: Arc::clone(&seeds),
        })));
        let factory = loaned_worker_factory(Arc::clone(&slot), |w: &mut Mock, seed| {
            w.seed_set.lock().unwrap().push(seed);
        });
        {
            let _loaned = factory(0, 42).expect("loan");
            assert!(slot.lock().unwrap().is_none(), "слот занят файлом");
            let err = match factory(1, 43) {
                Err(e) => format!("{e:#}"),
                Ok(_) => panic!("ожидалась ошибка занятого слота"),
            };
            assert!(
                err.contains("занят"),
                "параллельный файл на том же слоте — ошибка: {err}"
            );
        } // Drop Loaned — воркер возвращается
        assert!(slot.lock().unwrap().is_some(), "воркер вернулся в слот");
        assert_eq!(*seeds.lock().unwrap(), vec![42], "per-file seed установлен");
        // следующий файл одалживает того же воркера
        let factory2 = loaned_worker_factory(Arc::clone(&slot), |w: &mut Mock, seed| {
            w.seed_set.lock().unwrap().push(seed);
        });
        let _l2 = factory2(0, 100).expect("повторный loan");
        drop(_l2);
        assert_eq!(*seeds.lock().unwrap(), vec![42, 100]);
    }

    /// Пустой (отравленный) слот — явная ошибка, а не молчаливый noop.
    #[test]
    fn loaned_factory_empty_slot_errors() {
        let slot: WorkerSlot<Mock> = Arc::new(Mutex::new(None));
        let factory = loaned_worker_factory(slot, |_w: &mut Mock, _s| {});
        let err = match factory(0, 1) {
            Err(e) => format!("{e:#}"),
            Ok(_) => panic!("ожидалась ошибка пустого слота"),
        };
        assert!(err.contains("отравлен"), "{err}");
    }

    /// spawn_workers: успехи + сбои раздельно, без повторов загрузки.
    #[test]
    fn spawn_workers_reports_success_and_failures() {
        let created = Arc::new(Mutex::new(0usize));
        let created_c = Arc::clone(&created);
        let spawned: SpawnedWorkers<Mock> = spawn_workers(
            3,
            move |wid, _seed| {
                let mut g = created_c.lock().unwrap();
                *g += 1;
                if wid == 1 {
                    anyhow::bail!("нет VRAM");
                }
                Ok(Mock {
                    id: wid as u32,
                    seed_set: Arc::new(Mutex::new(Vec::new())),
                })
            },
            42,
        );
        assert_eq!(spawned.loaded, 2);
        assert_eq!(spawned.slots.len(), 2);
        assert_eq!(spawned.load_ms.len(), 2);
        assert_eq!(spawned.failures.len(), 1);
        assert!(spawned.failures[0].1.contains("нет VRAM"));
        assert_eq!(
            *created.lock().unwrap(),
            3,
            "каждый воркер — ровно одна попытка"
        );
    }

    /// Динамическая очередь: 2 воркера × 5 файлов — все файлы ровно один раз,
    /// результаты в исходном порядке, распределение динамическое.
    #[test]
    fn dynamic_queue_no_loss_no_duplication_in_order() {
        let slots: Vec<WorkerSlot<u8>> = (0..2).map(|i| Arc::new(Mutex::new(Some(i)))).collect();
        let cancel = Arc::new(AtomicBool::new(false));
        let outcome = run_dynamic_queue(slots, 5, &cancel, |pos, _wid, _slot| pos * 10);
        assert_eq!(
            outcome.results,
            vec![Some(0), Some(10), Some(20), Some(30), Some(40)],
            "результаты строго по позициям"
        );
        assert!(outcome.worker_fatal.is_empty());
        let all: Vec<usize> = outcome.claimed.concat();
        let mut sorted = all.clone();
        sorted.sort_unstable();
        assert_eq!(
            sorted,
            vec![0, 1, 2, 3, 4],
            "нет потерь и повторов: {all:?}"
        );
        // распределение динамическое: с мгновенными job один воркер может
        // забрать всё — единственная гарантия это полнота/уникальность выше
        // (совместную работу проверяет panic-тест: сосед подхватывает очередь)
        assert!(!all.is_empty());
    }

    /// Отмена останавливает выдачу: незапущенные файлы — None, зависаний нет.
    #[test]
    fn dynamic_queue_cancel_stops_dispatch() {
        let slots: Vec<WorkerSlot<u8>> = (0..2).map(|i| Arc::new(Mutex::new(Some(i)))).collect();
        let cancel = Arc::new(AtomicBool::new(true));
        let t0 = Instant::now();
        let outcome = run_dynamic_queue(slots, 6, &cancel, |pos, _w, _s| pos);
        assert!(t0.elapsed().as_secs() < 5, "без зависания");
        assert!(
            outcome.results.iter().all(|r| r.is_none()),
            "ни один файл не запущен"
        );
    }

    /// Паника job: слот отравлен (Pocket не переиспользуется), фатальная
    /// ошибка несёт позицию файла, остальные файлы доделывают живые воркеры.
    #[test]
    fn dynamic_queue_panic_poisons_slot_and_records_fatal() {
        let slots: Vec<WorkerSlot<u8>> = (0..2).map(|i| Arc::new(Mutex::new(Some(i)))).collect();
        let slot_copies: Vec<WorkerSlot<u8>> = slots.iter().map(Arc::clone).collect();
        let cancel = Arc::new(AtomicBool::new(false));
        let outcome = run_dynamic_queue(slots, 6, &cancel, |pos, _wid, _slot| {
            if pos == 1 {
                panic!("движок сломался");
            }
            pos
        });
        // файл 1 не завершён; остальные — кем-то из живых воркеров
        assert!(outcome.results[1].is_none(), "паника = нет результата");
        let done: usize = outcome.results.iter().filter(|r| r.is_some()).count();
        assert_eq!(done, 5, "остальные файлы доделаны: {:?}", outcome.results);
        assert_eq!(outcome.worker_fatal.len(), 1);
        assert_eq!(outcome.worker_fatal[0].2, "движок сломался");
        // один из слотов отравлен (None) — Pocket паниковавшего не возвращается
        assert!(
            slot_copies.iter().any(|s| s.lock().unwrap().is_none()),
            "слот паниковавшего воркера отравлен"
        );
    }

    /// Файлов меньше, чем воркеров: лишние потоки тихо выходят.
    #[test]
    fn dynamic_queue_more_workers_than_files() {
        let slots: Vec<WorkerSlot<u8>> = (0..4).map(|i| Arc::new(Mutex::new(Some(i)))).collect();
        let cancel = Arc::new(AtomicBool::new(false));
        let outcome = run_dynamic_queue(slots, 1, &cancel, |pos, _w, _s| pos + 1);
        assert_eq!(outcome.results, vec![Some(1)]);
        assert!(outcome.worker_fatal.is_empty());
    }

    /// Сквозной прогон: постоянный слот + run_job(workers=1) на каждый файл —
    /// модель переживает файлы, seed устанавливается на каждый файл.
    #[test]
    fn persistent_slot_survives_run_job_files() {
        let seeds = Arc::new(Mutex::new(Vec::new()));
        let slot: WorkerSlot<Mock> = Arc::new(Mutex::new(Some(Mock {
            id: 3,
            seed_set: Arc::clone(&seeds),
        })));
        let cancel = AtomicBool::new(false);
        for file_seed in [42u64, 43, 44] {
            let factory = loaned_worker_factory(Arc::clone(&slot), |w: &mut Mock, seed| {
                w.seed_set.lock().unwrap().push(seed);
            });
            let mut out = Vec::new();
            let summary = run_job(
                &PoolConfig {
                    workers: 1,
                    queue_cap: 2,
                },
                tasks(3),
                factory,
                |_i, pcm, _p| {
                    out.push(pcm[0] as u32);
                    Ok(())
                },
                &cancel,
                SeedPolicy::Fixed(file_seed),
            )
            .unwrap();
            assert_eq!(summary.chunks_done, 3);
            assert_eq!(out, vec![3, 3, 3], "тот же Pocket обслужил все чанки файла");
            assert!(
                slot.lock().unwrap().is_some(),
                "после файла воркер снова в слоте"
            );
        }
        assert_eq!(*seeds.lock().unwrap(), vec![42, 43, 44], "per-file seed");
    }
}

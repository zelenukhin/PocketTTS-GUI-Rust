//! SRT-конвейер поверх существующего пула и AudioSink — этапы SRT.4/SRT.5,
//! контракт docs/SRT_0_AUDIT_CONTRACT.md §4–6, §9.
//!
//! Схема: parse_srt → strip_srt_tags (по реплике) → ДЕЙСТВУЮЩИЙ чанкер
//! (Standard `max_tokens` / TeraTTS Android `max_chunk_chars`) ПО РЕПЛИКЕ —
//! границы реплик имеют приоритет, чанк никогда не объединяет две реплики —
//! плоские задачи пула (индексы 0..N; sink получает `pause_s = None`:
//! паузами владеет сборщик SRT) → `run_job` (воркеры, backpressure, отмена)
//! → сборка реплики строго по порядку (PCM чанков + внутренние паузы §6) →
//! `trim_edges` реплики → `SrtTimingPlanner` (тишина/темп) → SoundTouch →
//! ОДИН sink на файл (`write_chunk(seq, pcm, None)`; тишина — блоками
//! нулей точной длины в сэмплах) → фазы/публикация G5.1 без изменений.
//! Временные WAV/concat-файлы отсутствуют by construction; полный трек в
//! памяти не накапливается (буфер — одна реплика, предел
//! `max_cue_pcm_samples`).
//!
//! Внутренние паузы (§6): Standard — плоская политика SRT (мягкая граница
//! `comma_pause_ms`, обычная `internal_pause_ms`; дефолты 100/500 как в
//! Python); Android — собственный план пауз чанкера (`Task::pause_s`
//! задач SRT в sink НЕ попадает — его читает только сборщик). Последний
//! чанк реплики всегда без паузы. `paragraph_pause_s` в SRT-пути не
//! применяется.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;

use shtorm_engine::dsp::trim_edges;
use shtorm_engine::POCKET_RATE;
use text_processing::{
    android_chunk_pipeline, apply_char_rules, to_model_stress_notation, AndroidChunkOptions,
    ChunkingMode, SrtParseResult, SubtitleCue, CALL_CHUNK_LIMIT,
};

use crate::pipeline::{PipelineOptions, RunPhase, RunStatus, StreamError};
use crate::pool::{run_job, ChunkWorker, PoolConfig, SeedPolicy, Task};
use crate::sink::AudioSink;
use crate::soundtouch::{process_cue, SoundTouchLib};
use crate::srt::{CueDiagnostics, SrtTimingParams};

/// Настройки SRT-конвейера (GUI-блок «SRT»).
#[derive(Debug, Clone)]
pub struct SrtOptions {
    /// Тайминги: потолок ускорения, минимальная пауза, частота.
    pub timing: SrtTimingParams,
    /// Standard: внутренняя пауза между чанками реплики, мс (дефолт 500).
    pub internal_pause_ms: u32,
    /// Standard: пауза на мягкой границе (запятая/«и»), мс (дефолт 100).
    pub comma_pause_ms: u32,
    /// Предел буфера одной реплики в сэмплах (антивсплеск памяти;
    /// 15 минут × 24 кГц по контракту §9).
    pub max_cue_pcm_samples: u64,
}

impl Default for SrtOptions {
    fn default() -> Self {
        Self {
            timing: SrtTimingParams::default(),
            internal_pause_ms: 500,
            comma_pause_ms: 100,
            max_cue_pcm_samples: 15 * 60 * POCKET_RATE as u64,
        }
    }
}

/// Одна реплика в плане файла: границы её чанков в плоском списке задач.
#[derive(Debug, Clone)]
pub struct SrtCuePlan {
    /// Исходная реплика (индекс SRT, таймкоды в мс, исходный текст).
    pub cue: SubtitleCue,
    /// false — пустая после очистки: не озвучивается, но её таймкоды
    /// участвуют в расчёте пауз (Python-семантика).
    pub voiced: bool,
    /// Полуинтервал [start, end) позиций в `SrtFilePlan::tasks`
    /// (пустой у невыполняемой реплики).
    pub task_range: std::ops::Range<usize>,
}

/// План SRT-файла: реплики + плоские задачи пула (порядок реплик).
#[derive(Debug, Default)]
pub struct SrtFilePlan {
    /// Тексты чанков для журнала (позиция == task.index).
    pub texts: Vec<String>,
    /// Задачи пула; в sink у всех пауза None — паузы вставляет сборщик SRT.
    /// В Android-режиме `pause_s` несёт ВНУТРЕННЮЮ паузу чанкера для
    /// сборщика (в sink не попадает).
    pub tasks: Vec<Task>,
    /// Standard: флаги мягкой границы чанков (позиция == task.index).
    pub standard_soft: Vec<bool>,
    /// Реплики в порядке индексов SRT.
    pub cues: Vec<SrtCuePlan>,
    /// Заметки плана (пропущенные чанки Android, сцены и т.п.).
    pub notes: Vec<String>,
    /// Предупреждения парсера (хронология/перекрытия/пропуски).
    pub warnings: Vec<String>,
}

/// Построить план SRT-файла: парсинг, очистка, чанкование каждой реплики
/// действующим алгоритмом (`opts.chunking`), model_text-преобразования как
/// в TXT-пути (`to_model_stress_notation` + char_rules для Standard;
/// метаданные Android). Границы реплик приоритетнее чанкера: чанк никогда
/// не содержит текст двух реплик. Фатально: файл без реплик.
pub fn build_srt_tasks(parsed: &SrtParseResult, opts: &PipelineOptions) -> Result<SrtFilePlan> {
    anyhow::ensure!(
        !parsed.cues.is_empty(),
        "в SRT нет ни одной реплики с текстом и таймкодом"
    );
    let mut plan = SrtFilePlan {
        warnings: parsed.warnings.clone(),
        ..Default::default()
    };

    match opts.chunking {
        ChunkingMode::Standard => {
            for cue in &parsed.cues {
                let clean = text_processing::strip_srt_tags(&cue.text);
                let start = plan.tasks.len();
                if clean.is_empty() {
                    plan.cues.push(SrtCuePlan {
                        cue: cue.clone(),
                        voiced: false,
                        task_range: start..start,
                    });
                    continue;
                }
                let chunks = text_processing::chunk_pipeline(&clean, opts.max_tokens);
                anyhow::ensure!(
                    !chunks.is_empty(),
                    "реплика [{}] не нарезается на чанки",
                    cue.index
                );
                let cue_len = chunks.len();
                for chunk in &chunks {
                    let mut model_text = to_model_stress_notation(&chunk.text);
                    if let Some(rules) = &opts.char_rules {
                        model_text = apply_char_rules(&model_text, rules);
                    }
                    plan.texts.push(chunk.text.clone());
                    plan.standard_soft.push(chunk.soft_break);
                    plan.tasks.push(Task {
                        index: plan.tasks.len() as u64,
                        model_text,
                        pause_s: None,
                    });
                }
                // `is_last` последнего чанка реплики не используется:
                // хвостовая пауза реплики управляется таймингами SRT, не
                // настройками абзаца (Python `_collect_srt_subtitles`
                // ставит всем чанкам is_last=False).
                let _ = cue_len;
                plan.cues.push(SrtCuePlan {
                    cue: cue.clone(),
                    voiced: true,
                    task_range: start..plan.tasks.len(),
                });
            }
        }
        ChunkingMode::TeraTtsAndroid => {
            anyhow::ensure!(
                opts.max_chunk_chars >= 1,
                "max_chunk_chars должен быть ≥ 1 (получено {})",
                opts.max_chunk_chars
            );
            let a_opts = AndroidChunkOptions {
                max_chunk_chars: opts.max_chunk_chars as usize,
                paragraph_pause_s: opts.paragraph_pause_s,
                pause_scale: opts.pause_scale,
                call_chunk_limit: CALL_CHUNK_LIMIT,
            };
            for cue in &parsed.cues {
                let clean = text_processing::strip_srt_tags(&cue.text);
                let start = plan.tasks.len();
                if clean.is_empty() {
                    plan.cues.push(SrtCuePlan {
                        cue: cue.clone(),
                        voiced: false,
                        task_range: start..start,
                    });
                    continue;
                }
                // Android-план строится ПО РЕПЛИКЕ (границы SRT приоритетны);
                // внутри реплики — действующая семантика: сцены вливают паузу
                // в предыдущий чанк, чанки без кириллицы пропускаются с
                // переносом паузы (как build_file_tasks, но в пределах
                // реплики). Внутренняя пауза хранится в Task::pause_s для
                // сборщика; sink её не видит.
                let android_plan = android_chunk_pipeline(&clean, &a_opts);
                let mut texts: Vec<String> = Vec::new();
                let mut models: Vec<String> = Vec::new();
                let mut pauses: Vec<Option<f32>> = Vec::new();
                for c in android_plan {
                    if c.is_scene() {
                        if let Some(sp) = c.pause_s {
                            match pauses.last_mut() {
                                Some(Some(p)) => *p += sp,
                                Some(slot) => *slot = Some(sp),
                                None => plan.notes.push(format!(
                                    "реплика [{}]: сцена-разделитель в начале реплики — ведущая пауза {sp:.2} с не вставляется",
                                    cue.index
                                )),
                            }
                        }
                        continue;
                    }
                    match c.model_text {
                        Some(m) => {
                            texts.push(c.text.clone());
                            models.push(m);
                            pauses.push(c.pause_s);
                        }
                        None => {
                            if let Some(p) = c.pause_s {
                                match pauses.last_mut() {
                                    Some(Some(lp)) => *lp += p,
                                    Some(slot) => *slot = Some(p),
                                    None => plan.notes.push(format!(
                                        "реплика [{}]: чанк в начале реплики пропущен (нет кириллицы): {:?} — пауза {p:.2} с не вставляется",
                                        cue.index, c.text
                                    )),
                                }
                            }
                            plan.notes
                                .push(format!("(пропущено: нет кириллицы) {}", c.text));
                        }
                    }
                }
                let voiced = !models.is_empty();
                if voiced {
                    // Последний чанк реплики — без паузы (тайминги SRT).
                    let last = pauses.len() - 1;
                    pauses[last] = None;
                    for ((model, text), pause) in models.into_iter().zip(texts).zip(pauses) {
                        plan.texts.push(text);
                        plan.tasks.push(Task {
                            index: plan.tasks.len() as u64,
                            model_text: model,
                            pause_s: pause,
                        });
                    }
                }
                plan.cues.push(SrtCuePlan {
                    cue: cue.clone(),
                    voiced,
                    task_range: start..plan.tasks.len(),
                });
            }
        }
    }
    Ok(plan)
}

/// Итог SRT-прогона (аналог StreamOutcome для SRT-режима).
#[derive(Debug)]
pub struct SrtOutcome {
    pub status: RunStatus,
    pub path: Option<std::path::PathBuf>,
    pub partial_path: std::path::PathBuf,
    pub workers: usize,
    pub base_seed: u64,
    pub chunks_done: usize,
    pub total_chunks: usize,
    pub cues_total: usize,
    pub cues_voiced: usize,
    pub sample_rate: u32,
    /// Речь реплик (без тишин/заимствований), сэмплы.
    pub speech_samples: u64,
    /// Диагностика таймингов по репликам (контракт §8.1) для лога.
    pub timing_rows: Vec<CueDiagnostics>,
    pub plan_notes: Vec<String>,
    pub warnings: Vec<String>,
    pub max_buffered: usize,
    pub max_buffered_bytes: usize,
    /// Пик длины буфера ОДНОЙ реплики (сэмплы): подтверждение соблюдения
    /// `max_cue_pcm_samples` (SRT.9A — проверка до накопления).
    pub max_cue_pcm_len: usize,
}

/// Сборщик реплик: замыкание приёмника пула. Собирает PCM чанков реплики
/// строго по порядку, вставляет внутренние паузы, на завершении реплики —
/// обрезка, план таймингов, SoundTouch, тишина, запись в sink.
struct SrtAssembler<'a> {
    /// (start, end) диапазоны задач по репликам плана.
    plan_ranges: Vec<(usize, usize)>,
    /// (индекс SRT, start, end, next_start) реплик в сэмплах.
    cue_infos: Vec<(i64, u64, u64, Option<u64>)>,
    voiced: Vec<bool>,
    /// Внутренняя пауза после чанка i (сэмплы), 0 — последний чанк реплики.
    pause_after_chunk: Vec<u64>,
    planner: crate::srt::SrtTimingPlanner,
    sink: &'a mut dyn AudioSink,
    soundtouch: Option<&'a SoundTouchLib>,
    max_cue_pcm_samples: u64,
    /// Буфер текущей реплики (PCM + внутренние паузы).
    cue_pcm: Vec<f32>,
    /// Номер реплики, ожидаемой сборщиком (первая выполняемая).
    current_cue: usize,
    timing_rows: Vec<CueDiagnostics>,
    /// Счётчик записей в sink (index для sink.write_chunk).
    sink_seq: usize,
    silence_scratch: Vec<f32>,
    speech_samples: u64,
    /// Пик длины буфера реплики (диагностика соблюдения лимита SRT.9A).
    max_cue_len: usize,
}

/// SRT.9A: проверка предела буфера реплики ДО какого-либо расширения.
/// Возвращает итоговую длину (для `resize` внутренней паузы); превышение
/// или переполнение счётчика — явная ошибка с требуемым/разрешённым
/// размером, без копирования входных данных и без роста буфера.
fn ensure_cue_capacity(cur: u64, add: u64, limit: u64, cue_index: i64) -> Result<usize> {
    let need = cur.checked_add(add).ok_or_else(|| {
        anyhow::anyhow!(
            "реплика [{cue_index}] превышает ограничение PCM-буфера: \
             переполнение счётчика сэмплов ({cur}+{add})"
        )
    })?;
    anyhow::ensure!(
        need <= limit,
        "реплика [{cue_index}] превышает ограничение PCM-буфера: требуется {need} сэмплов, разрешено {limit}"
    );
    usize::try_from(need).map_err(|_| {
        anyhow::anyhow!(
            "реплика [{cue_index}]: длина буфера {need} сэмплов не поддерживается платформой"
        )
    })
}

impl<'a> SrtAssembler<'a> {
    /// Приём чанка из пула (строго по порядку индексов).
    fn on_chunk(&mut self, index: usize, pcm: &[f32]) -> Result<()> {
        let cue_no = self
            .cue_of_task(index)
            .expect("задача вне диапазонов реплик");
        debug_assert!(self.voiced[cue_no]);
        anyhow::ensure!(
            cue_no == self.current_cue,
            "внутренняя ошибка сборки: ожидалась реплика {}, пришёл чанк {}",
            self.current_cue,
            index
        );
        // SRT.9A: лимит реплики действует ДО накопления — входной блок
        // копируется только если итог укладывается в предел.
        let cue_index = self.cue_infos[cue_no].0;
        let after_pcm = ensure_cue_capacity(
            self.cue_pcm.len() as u64,
            pcm.len() as u64,
            self.max_cue_pcm_samples,
            cue_index,
        )?;
        self.cue_pcm.extend_from_slice(pcm);
        self.max_cue_len = self.max_cue_len.max(self.cue_pcm.len());
        let end = self.plan_ranges[cue_no].1;
        if index + 1 == end {
            self.finish_cue(cue_no)?;
            // Следующая ОЖИДАЕМАЯ реплика — следующая выполняемая по списку
            // (невыполняемые перескакиваются: их окно поглотится тишиной).
            loop {
                self.current_cue += 1;
                if self.current_cue >= self.voiced.len() || self.voiced[self.current_cue] {
                    break;
                }
            }
        } else {
            // Внутренняя пауза — тоже до `resize`: тишина не может
            // вытолкнуть буфер за предел (раньше это проверялось лишь
            // после сборки всей реплики).
            let pause = self.pause_after_chunk[index];
            let new_len =
                ensure_cue_capacity(after_pcm as u64, pause, self.max_cue_pcm_samples, cue_index)?;
            self.cue_pcm.resize(new_len, 0.0);
            self.max_cue_len = self.max_cue_len.max(new_len);
        }
        Ok(())
    }

    fn cue_of_task(&self, index: usize) -> Option<usize> {
        let mut c = self.current_cue;
        while c < self.plan_ranges.len() {
            let (s, e) = self.plan_ranges[c];
            if index < e {
                return if index >= s && self.voiced[c] {
                    Some(c)
                } else {
                    None
                };
            }
            c += 1;
        }
        None
    }

    /// Завершение реплики: обрезка → план → тишина → темп → sink → commit.
    fn finish_cue(&mut self, cue_no: usize) -> Result<()> {
        let pcm = std::mem::take(&mut self.cue_pcm);
        // Финальная обрезка реплики (аналог trim_wav_edges(raw_path) в
        // _assemble_srt_track; чанки уже обрезаны воркером). Копия `pcm`
        // больше не нужна — освобождаем до SoundTouch (SRT.9A: пик памяти
        // на реплику — буфер + обрезанная копия, не три копии).
        let trimmed = trim_edges(&pcm, 0.02, 0.10, POCKET_RATE);
        drop(pcm);
        // SRT.9A: основная проверка лимита — в on_chunk ДО накопления;
        // здесь остаётся страховка (обрезка только укорачивает, поэтому
        // при соблюдении инварианта недостижима).
        anyhow::ensure!(
            trimmed.len() as u64 <= self.max_cue_pcm_samples,
            "реплика [{}] слишком длинная после синтеза ({} сэмплов > лимита {})",
            self.cue_infos[cue_no].0,
            trimmed.len(),
            self.max_cue_pcm_samples
        );
        let raw_samples = trimmed.len() as u64;
        let (cue_index, start, end, next_start) = self.cue_infos[cue_no];
        let plan = self.planner.plan_cue(start, end, raw_samples, next_start);

        // Тишина до начала реплики — точными блоками нулей (без f32-секунд:
        // позиции трека остаются целочисленно точными).
        self.write_silence(plan.silence_before)?;

        let out = match plan.tempo {
            Some(t) => {
                let lib = self.soundtouch.ok_or_else(|| {
                    anyhow::anyhow!(
                        "SoundTouchDLL.dll не найдена — ускорение реплики [{}] невозможно",
                        cue_index
                    )
                })?;
                process_cue(lib, &trimmed, t, POCKET_RATE, 1).map_err(|e| {
                    anyhow::anyhow!("SoundTouch не смог обработать реплику [{cue_index}]: {e}")
                })?
            }
            None => trimmed,
        };
        self.speech_samples += raw_samples;
        let actual = out.len() as u64;
        self.sink.write_chunk(self.sink_seq, &out, None)?;
        self.sink_seq += 1;
        self.planner.commit(actual);
        self.timing_rows.push(self.planner.cue_diagnostics(
            cue_index,
            start,
            end,
            raw_samples,
            &plan,
            actual,
        ));
        Ok(())
    }

    fn write_silence(&mut self, samples: u64) -> Result<()> {
        const BLOCK: u64 = 30 * POCKET_RATE as u64; // блоки по 30 с
        let mut left = samples;
        while left > 0 {
            let n = left.min(BLOCK) as usize;
            self.silence_scratch.resize(n, 0.0);
            self.sink
                .write_chunk(self.sink_seq, &self.silence_scratch, None)?;
            self.sink_seq += 1;
            left -= n as u64;
        }
        Ok(())
    }
}

/// Полный SRT-прогон: план → пул воркеров (инъекция фабрики — RealWorker
/// или мок) → сборка реплик → один sink → фазы G5.1 (Generating →
/// Finalizing → Publishing) с единой точкой решения о публикации.
#[allow(clippy::too_many_arguments)]
pub fn synthesize_srt_stream<W, FacW, F>(
    srt_content: &str,
    opts: &PipelineOptions,
    srt: &SrtOptions,
    workers: usize,
    mut sink: Box<dyn AudioSink>,
    soundtouch: Option<&SoundTouchLib>,
    mut progress: F,
    cancel: &AtomicBool,
    phase: &dyn Fn(RunPhase),
    factory: FacW,
) -> Result<SrtOutcome>
where
    W: ChunkWorker + 'static,
    FacW: Fn(usize, u64) -> Result<W> + Send + Sync + 'static,
    F: FnMut(usize, usize),
{
    let parsed = text_processing::parse_srt(srt_content);
    let file_plan = build_srt_tasks(&parsed, opts)?;
    let total = file_plan.tasks.len();
    let cues_total = file_plan.cues.len();
    anyhow::ensure!(total > 0, "в SRT нет чанков для синтеза");
    let partial_path = sink.partial_path();
    let fs = POCKET_RATE;

    // Тайминги реплик в сэмплах (точное ×24 при 24 кГц); next — следующая
    // реплика СПИСКА, включая невыполняемые (Python-семантика заимствования).
    let ms = |v: u64| srt.timing.ms_to_samples(v);
    let cue_infos: Vec<(i64, u64, u64, Option<u64>)> = file_plan
        .cues
        .iter()
        .enumerate()
        .map(|(i, cp)| {
            (
                cp.cue.index,
                ms(cp.cue.start_ms),
                ms(cp.cue.end_ms),
                file_plan.cues.get(i + 1).map(|n| ms(n.cue.start_ms)),
            )
        })
        .collect();

    // Внутренние паузы по чанкам: Standard — плоская политика SRT по флагу
    // soft_break плана; Android — pause_s задач. Последний чанк реплики — 0.
    let mut pause_after_chunk: Vec<u64> = vec![0; total];
    for cp in &file_plan.cues {
        if !cp.voiced {
            continue;
        }
        for pos in cp.task_range.clone() {
            if pos + 1 == cp.task_range.end {
                pause_after_chunk[pos] = 0;
            } else {
                pause_after_chunk[pos] = match opts.chunking {
                    ChunkingMode::Standard => {
                        let ms_pause = if file_plan.standard_soft[pos] {
                            srt.comma_pause_ms
                        } else {
                            srt.internal_pause_ms
                        };
                        srt.timing.ms_to_samples(ms_pause as u64)
                    }
                    ChunkingMode::TeraTtsAndroid => {
                        (file_plan.tasks[pos].pause_s.unwrap_or(0.0).max(0.0) * fs as f32) as u64
                    }
                };
            }
        }
    }

    let base_seed = match opts.seed {
        Some(s) => s,
        None => SeedPolicy::Random.resolve(),
    };
    let cfg = PoolConfig {
        workers: workers.max(1),
        queue_cap: 4,
    };

    phase(RunPhase::Generating);
    let mut done_ref = 0usize;
    let mut assembler = SrtAssembler {
        plan_ranges: file_plan
            .cues
            .iter()
            .map(|c| (c.task_range.start, c.task_range.end))
            .collect(),
        cue_infos,
        voiced: file_plan.cues.iter().map(|c| c.voiced).collect(),
        pause_after_chunk,
        planner: crate::srt::SrtTimingPlanner::new(srt.timing.clone()),
        sink: &mut *sink,
        soundtouch,
        max_cue_pcm_samples: srt.max_cue_pcm_samples,
        cue_pcm: Vec::new(),
        current_cue: file_plan.cues.iter().position(|c| c.voiced).unwrap_or(0),
        timing_rows: Vec::new(),
        sink_seq: 0,
        silence_scratch: Vec::new(),
        speech_samples: 0,
        max_cue_len: 0,
    };

    let run = run_job(
        &cfg,
        file_plan.tasks.clone(),
        factory,
        |i, pcm, _pause| {
            assembler.on_chunk(i, pcm)?;
            done_ref += 1;
            progress(done_ref, total);
            Ok(())
        },
        cancel,
        SeedPolicy::Fixed(base_seed),
    );
    let summary = match run {
        Ok(s) => s,
        Err(e) => {
            let se = StreamError {
                message: format!("{e:#}"),
                partial: partial_path.clone(),
                chunks_done: done_ref,
                total,
                base_seed,
                publish_failed: false,
            };
            return Err(anyhow::Error::new(se));
        }
    };

    // Завершение заимствования sink сборщиком: данные извлечены до
    // финализации (sink.finish() владеет Box).
    let timing_rows = std::mem::take(&mut assembler.timing_rows);
    let speech_samples = assembler.speech_samples;
    let max_cue_pcm_len = assembler.max_cue_len;
    drop(assembler);

    let mut status = if summary.cancelled {
        RunStatus::Cancelled
    } else if summary.chunks_done == total {
        RunStatus::Completed
    } else {
        RunStatus::Failed
    };
    if status == RunStatus::Completed && cancel.load(Ordering::SeqCst) {
        status = RunStatus::Cancelled;
    }

    let (path, final_partial) = match status {
        RunStatus::Completed => {
            phase(RunPhase::Finalizing);
            let fin = sink.finish().map_err(|e| {
                anyhow::Error::new(StreamError {
                    message: format!("не удалось завершить кодирование: {e:#}"),
                    partial: partial_path.clone(),
                    chunks_done: summary.chunks_done,
                    total,
                    base_seed,
                    publish_failed: false,
                })
            })?;
            phase(RunPhase::Publishing);
            let published = fin.publish().map_err(|e| {
                anyhow::Error::new(StreamError {
                    message: format!("ошибка публикации: {e:#}"),
                    partial: partial_path.clone(),
                    chunks_done: summary.chunks_done,
                    total,
                    base_seed,
                    publish_failed: true,
                })
            })?;
            (Some(published), partial_path)
        }
        RunStatus::Cancelled => (None, partial_path),
        RunStatus::Failed => {
            return Err(anyhow::Error::new(StreamError {
                message: "синтез не завершён (без отмены) — файл не создан".into(),
                partial: partial_path.clone(),
                chunks_done: summary.chunks_done,
                total,
                base_seed,
                publish_failed: false,
            }));
        }
    };

    Ok(SrtOutcome {
        status,
        path,
        partial_path: final_partial,
        workers: summary.workers,
        base_seed,
        chunks_done: summary.chunks_done,
        total_chunks: total,
        cues_total,
        cues_voiced: file_plan.cues.iter().filter(|c| c.voiced).count(),
        sample_rate: fs,
        speech_samples,
        timing_rows,
        plan_notes: file_plan.notes,
        warnings: file_plan.warnings,
        max_buffered: summary.max_buffered,
        max_buffered_bytes: summary.max_buffered_bytes,
        max_cue_pcm_len,
    })
}

/// Лог таймингов SRT — порт `_write_srt_timing_log` Python: `{stem}_log.txt`
/// рядом с результатом, create_new (как `write_chunk_log` TXT-пути).
pub fn write_srt_timing_log(
    out_path: &Path,
    source_name: &str,
    outcome: &SrtOutcome,
) -> Result<std::path::PathBuf> {
    use std::io::Write;
    let log_path = out_path.with_file_name(format!(
        "{}_log.txt",
        out_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("srt")
    ));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&log_path)?;
    let fs = outcome.sample_rate;
    let ms = |s: u64| s as f64 * 1000.0 / fs as f64;
    writeln!(f, "Лог таймингов SRT")?;
    writeln!(f, "Исходник: {source_name}")?;
    writeln!(
        f,
        "Озвучено реплик: {} из {} (чанков {})",
        outcome.timing_rows.len(),
        outcome.cues_total,
        outcome.total_chunks
    )?;
    for d in &outcome.timing_rows {
        writeln!(
            f,
            "[{}] {:.0}–{:.0} мс | синтез {:.0} мс -> финал {:.0} мс",
            d.cue_index,
            ms(d.start_samples),
            ms(d.end_samples),
            ms(d.raw_samples),
            ms(d.final_samples)
        )?;
        let mut detail = format!("лимит {:.0} мс", ms(d.allowable_samples));
        if d.borrow_samples > 0 {
            detail.push_str(&format!(
                " (заимствовано у паузы {:.0} мс)",
                ms(d.borrow_samples)
            ));
        }
        if let Some(t) = d.tempo {
            detail.push_str(&format!(", ускорение ×{t:.2}"));
        }
        if d.hard_lag {
            detail.push_str(", жёсткий лаг (<100 мс)");
        }
        if d.late_by_samples > 0 {
            detail.push_str(&format!(", опоздание {:.0} мс", ms(d.late_by_samples)));
        }
        writeln!(f, "    -> {detail}")?;
    }
    Ok(log_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::{ChunkAudio, DenoiseChunkStatus};
    use crate::sink::{make_sink, OutputFormat};
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;

    /// Мок-воркер: синус фиксированной длины (1 с + 0.1 с за «позицию»).
    struct FakeWorker;
    impl ChunkWorker for FakeWorker {
        fn process(&mut self, task: &Task) -> Result<ChunkAudio> {
            let len = 24_000 + (task.index as usize % 3) * 2_400;
            let pcm: Vec<f32> = (0..len).map(|i| ((i as f32) * 0.01).sin() * 0.5).collect();
            Ok(ChunkAudio {
                speech: pcm,
                pause_s: None,
                denoise: DenoiseChunkStatus::Off,
            })
        }
    }

    fn fake_factory(_wid: usize, _seed: u64) -> Result<FakeWorker> {
        Ok(FakeWorker)
    }

    fn std_opts() -> PipelineOptions {
        PipelineOptions {
            model_dir: PathBuf::from("."),
            voice: PathBuf::from("."),
            device: shtorm_engine::Device::Cpu,
            threads: 1,
            precision: shtorm_engine::ModelPrecision::Fp32,
            seed: Some(42),
            temperature: 0.3,
            decode_steps: 1,
            eos_threshold: -4.0,
            eos_tail_short: shtorm_engine::EosTail::Default,
            eos_tail_long: shtorm_engine::EosTail::Default,
            brightness_db: 0.0,
            denoise: false,
            pause_scale: 1.0,
            paragraph_pause_s: 0.9,
            max_tokens: 50,
            chunking: ChunkingMode::Standard,
            max_chunk_chars: 150,
            char_rules: None,
            anti_repeat: true,
        }
    }

    fn load_st() -> Option<SoundTouchLib> {
        SoundTouchLib::load(None).ok()
    }

    fn run_srt(
        srt: &str,
        sink: Box<dyn AudioSink>,
        srt_opts: &SrtOptions,
        st: Option<&SoundTouchLib>,
        cancel: &AtomicBool,
    ) -> Result<SrtOutcome> {
        synthesize_srt_stream(
            srt,
            &std_opts(),
            srt_opts,
            2,
            sink,
            st,
            |_d, _t| {},
            cancel,
            &|_| {},
            fake_factory,
        )
    }

    fn temp_wav(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir()
                .join("srt4_tests")
                .join(format!("{}_{}", tag, std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut p = dir.join("out.wav");
        let mut n = 0u32;
        while p.exists() {
            n += 1;
            p = dir.join(format!("out_{n}.wav"));
        }
        p
    }

    fn wav_duration(path: &Path) -> f64 {
        let b = std::fs::read(path).unwrap();
        let data = u32::from_le_bytes([b[40], b[41], b[42], b[43]]) as f64;
        data / 2.0 / 24_000.0
    }

    #[test]
    fn plan_standard_basic() {
        let parsed = text_processing::parse_srt(
            "1\n00:00:01,000 --> 00:00:03,000\nПривет, мир!\n\n2\n00:00:05,000 --> 00:00:06,000\nДа.\n",
        );
        let plan = build_srt_tasks(&parsed, &std_opts()).unwrap();
        assert_eq!(plan.cues.len(), 2);
        assert!(plan.cues.iter().all(|c| c.voiced));
        assert_eq!(plan.tasks.len(), 2); // короткие реплики — по 1 чанку
        assert_eq!(plan.tasks[0].model_text, "Привет, мир!");
        assert!(plan.tasks.iter().all(|t| t.pause_s.is_none()));
        assert_eq!(plan.cues[0].task_range, 0..1);
        assert_eq!(plan.cues[1].task_range, 1..2);
        assert_eq!(plan.standard_soft.len(), 2);
    }

    #[test]
    fn plan_strips_tags_and_empty_cue_unvoiced() {
        let parsed = text_processing::parse_srt(
            "1\n00:00:01,000 --> 00:00:02,000\n<i></i>\n\n2\n00:00:03,000 --> 00:00:04,000\n{\\an8}Речь наверху\n",
        );
        let plan = build_srt_tasks(&parsed, &std_opts()).unwrap();
        assert_eq!(plan.cues.len(), 2);
        assert!(!plan.cues[0].voiced);
        assert_eq!(plan.cues[0].task_range.len(), 0);
        assert!(plan.cues[1].voiced);
        assert_eq!(plan.tasks.len(), 1);
        // terminate_bare_chunks действующего Standard-чанкера добавляет
        // точку в конец одиночного «голого» чанка — семантика Rust-TXT.
        assert_eq!(plan.tasks[0].model_text, "Речь наверху.");
    }

    #[test]
    fn plan_long_cue_multiple_chunks() {
        let long_text = "Длинная реплика. ".repeat(40);
        let srt = format!(
            "1\n00:00:00,000 --> 00:01:00,000\n{long_text}\n\n2\n00:01:30,000 --> 00:01:31,000\nКратко.\n"
        );
        let parsed = text_processing::parse_srt(&srt);
        let plan = build_srt_tasks(&parsed, &std_opts()).unwrap();
        assert!(
            plan.tasks.len() > 2,
            "реплика режется: {}",
            plan.tasks.len()
        );
        assert_eq!(plan.cues[0].task_range.end, plan.cues[1].task_range.start);
        for t in &plan.texts[plan.cues[0].task_range.clone()] {
            assert!(!t.contains("Кратко"), "чанк перетёк в соседнюю реплику");
        }
    }

    #[test]
    fn plan_android_mode_uses_metadata_pauses() {
        let parsed = text_processing::parse_srt(
            "1\n00:00:01,000 --> 00:00:30,000\nПервое предложение достаточно длинное, чтобы что-то было. Второе предложение реплики тоже есть.\n",
        );
        let mut opts = std_opts();
        opts.chunking = ChunkingMode::TeraTtsAndroid;
        opts.max_chunk_chars = 60;
        let plan = build_srt_tasks(&parsed, &opts).unwrap();
        assert!(plan.cues[0].voiced);
        assert!(!plan.tasks.is_empty());
        let last = plan.cues[0].task_range.end - 1;
        assert_eq!(
            plan.tasks[last].pause_s, None,
            "последний чанк реплики без паузы"
        );
        // внутренние паузы — из метаданных Android (не плоские 500 мс)
        if plan.tasks.len() > 1 {
            let inner = plan.tasks[0].pause_s;
            assert!(
                inner.is_none() || inner.unwrap() < 0.5,
                "Android-пауза из метаданных: {:?}",
                inner
            );
        }
    }

    #[test]
    fn empty_srt_is_error() {
        let parsed = text_processing::parse_srt("");
        assert!(build_srt_tasks(&parsed, &std_opts()).is_err());
    }

    #[test]
    fn wav_stream_basic_with_lead_silence() {
        // ТЗ §12.D: WAV без промежуточных файлов; реплика 2 начинается
        // на ~5 с — тишина до неё вставлена точно.
        let st = load_st();
        let cancel = AtomicBool::new(false);
        let out = temp_wav("basic");
        let sink = make_sink(&out, 24_000, OutputFormat::Wav, "", None).unwrap();
        // окно реплики 2 (5.0..6.3 с) достаточное — без жёсткого лага/темпа
        let srt = "1\n00:00:00,500 --> 00:00:01,000\nПервый.\n\n2\n00:00:05,000 --> 00:00:06,300\nВторой.\n";
        let r = run_srt(srt, sink, &SrtOptions::default(), st.as_ref(), &cancel).unwrap();
        assert_eq!(r.status, RunStatus::Completed);
        let published = r.path.expect("published");
        let dur = wav_duration(&published);
        // тишина 0.5с + речь ~1с + тишина до 5с + речь ~1.1с (±trim)
        assert!((5.8..=7.0).contains(&dur), "длительность {dur}");
        assert_eq!(
            r.timing_rows[1].tempo, None,
            "окно достаточное — без ускорения"
        );
        assert_eq!(r.timing_rows.len(), 2);
        assert_eq!(r.cues_voiced, 2);
        let d2 = &r.timing_rows[1];
        let start2_ms = d2.speech_start_samples as f64 / 24.0;
        assert!(
            (4700.0..=5300.0).contains(&start2_ms),
            "старт второй {start2_ms} мс"
        );
        assert!(out.exists());
    }

    #[test]
    fn empty_middle_cue_gap_flows_to_next() {
        // Пустая реплика в середине: не озвучивается, тайминги участвуют.
        let st = load_st();
        let cancel = AtomicBool::new(false);
        let out = temp_wav("midempty");
        let sink = make_sink(&out, 24_000, OutputFormat::Wav, "", None).unwrap();
        let srt = "1\n00:00:00,000 --> 00:00:00,500\nА.\n\n2\n00:00:01,000 --> 00:00:01,500\n<i></i>\n\n3\n00:00:03,000 --> 00:00:03,500\nВ.\n";
        let r = run_srt(srt, sink, &SrtOptions::default(), st.as_ref(), &cancel).unwrap();
        assert_eq!(r.status, RunStatus::Completed);
        assert_eq!(r.cues_total, 3);
        assert_eq!(r.cues_voiced, 2);
        assert_eq!(r.timing_rows.len(), 2);
        // третья реплика стартует ~на 3 с (тишина поглотила пустую)
        let d = &r.timing_rows[1];
        let start_ms = d.speech_start_samples as f64 / 24.0;
        assert!(
            (2800.0..=3200.0).contains(&start_ms),
            "старт третьей {start_ms} мс"
        );
    }

    #[test]
    fn cancel_before_publish_leaves_no_result() {
        let st = load_st();
        let cancel = AtomicBool::new(true); // отменён ДО запуска
        let out = temp_wav("cancel");
        let sink = make_sink(&out, 24_000, OutputFormat::Wav, "", None).unwrap();
        let srt = "1\n00:00:00,000 --> 00:00:02,000\nОтмена.\n";
        let r = run_srt(srt, sink, &SrtOptions::default(), st.as_ref(), &cancel).unwrap();
        assert_eq!(r.status, RunStatus::Cancelled);
        assert!(r.path.is_none());
        assert!(!out.exists(), "готовый файл не публикуется");
    }

    #[test]
    fn tempo_required_but_dll_missing_is_error() {
        // Реплика длиннее окна → нужен темп, DLL не передана → явная ошибка.
        let cancel = AtomicBool::new(false);
        let out = temp_wav("nodll");
        let sink = make_sink(&out, 24_000, OutputFormat::Wav, "", None).unwrap();
        // Фейк-речь ~1.0-1.2 с; окно 0.5 с, зазор до следующей 0 → tempo.
        let srt = "1\n00:00:00,000 --> 00:00:00,500\nСлишком длинная реплика для короткого окна.\n";
        let r = run_srt(srt, sink, &SrtOptions::default(), None, &cancel);
        match r {
            Err(e) => {
                let msg = format!("{e:#}");
                assert!(msg.contains("SoundTouchDLL"), "сообщение: {msg}");
            }
            Ok(o) => {
                // После trim речь могла влезть — тогда tempo не нужен.
                assert!(o.timing_rows[0].tempo.is_none());
            }
        }
        assert!(!out.exists());
    }

    #[test]
    fn memory_limit_cue_is_error() {
        // Сверхмалый лимит буфера реплики → явная ошибка ДО накопления
        // (SRT.9A: раньше проверялось только после сборки реплики).
        let st = load_st();
        let cancel = AtomicBool::new(false);
        let out = temp_wav("memlimit");
        let sink = make_sink(&out, 24_000, OutputFormat::Wav, "", None).unwrap();
        let srt = "1\n00:00:00,000 --> 00:00:05,000\nРеплика.\n";
        let srt_opts = SrtOptions {
            max_cue_pcm_samples: 100,
            ..Default::default()
        };
        let r = run_srt(srt, sink, &srt_opts, st.as_ref(), &cancel);
        assert!(r.is_err());
        let msg = format!("{:#}", r.err().unwrap());
        assert!(msg.contains("превышает ограничение"), "сообщение: {msg}");
        assert!(msg.contains("разрешено 100"), "сообщение: {msg}");
        assert!(!out.exists(), "неполный результат не публикуется");
    }

    /// SRT.9A: воркер фиксированной длины PCM (для тестов предела буфера).
    struct FixedLenWorker(usize);
    impl ChunkWorker for FixedLenWorker {
        fn process(&mut self, _task: &Task) -> Result<ChunkAudio> {
            let pcm: Vec<f32> = vec![0.3; self.0];
            Ok(ChunkAudio {
                speech: pcm,
                pause_s: None,
                denoise: DenoiseChunkStatus::Off,
            })
        }
    }

    fn fixed_factory(len: usize) -> impl Fn(usize, u64) -> Result<FixedLenWorker> {
        move |_wid, _seed| Ok(FixedLenWorker(len))
    }

    fn run_srt_fixed(
        tag: &str,
        srt: &str,
        srt_opts: &SrtOptions,
        chunk_len: usize,
        cancel: &AtomicBool,
    ) -> Result<SrtOutcome> {
        let sink = make_sink(&temp_wav(tag), 24_000, OutputFormat::Wav, "", None).unwrap();
        let st = load_st();
        synthesize_srt_stream(
            srt,
            &std_opts(),
            srt_opts,
            2,
            sink,
            st.as_ref(),
            |_d, _t| {},
            cancel,
            &|_| {},
            fixed_factory(chunk_len),
        )
    }

    fn opts_with_limit(limit: u64) -> SrtOptions {
        SrtOptions {
            max_cue_pcm_samples: limit,
            ..Default::default()
        }
    }

    /// SRT.9A-1: реплика ровно по лимиту — принимается.
    #[test]
    fn cue_at_exact_limit_accepted() {
        let cancel = AtomicBool::new(false);
        let len = 24_000usize;
        let r = run_srt_fixed(
            "exact",
            "1\n00:00:00,000 --> 00:00:05,000\nРовно.\n",
            &opts_with_limit(len as u64),
            len,
            &cancel,
        )
        .unwrap();
        assert_eq!(r.status, RunStatus::Completed);
        assert_eq!(r.timing_rows.len(), 1);
        assert_eq!(r.max_cue_pcm_len, len, "пик буфера равен лимиту");
    }

    /// SRT.9A-2: на один сэмпл больше лимита — ошибка ДО накопления
    /// (сообщение новой инварианты: требуется/разрешено).
    #[test]
    fn cue_one_sample_over_limit_is_error() {
        let cancel = AtomicBool::new(false);
        let len = 24_000usize;
        let r = run_srt_fixed(
            "over1",
            "1\n00:00:00,000 --> 00:00:05,000\nЛимит.\n",
            &opts_with_limit(len as u64 - 1),
            len,
            &cancel,
        );
        let err = format!("{:#}", r.expect_err("ошибка лимита"));
        assert!(
            err.contains("превышает ограничение PCM-буфера"),
            "сообщение: {err}"
        );
        assert!(
            err.contains("разрешено 23999"),
            "в сообщении есть разрешённый лимит: {err}"
        );
    }

    /// SRT.9A-3: превышение одним большим входным чанком — ошибка до
    /// копирования PCM в буфер реплики; результат не публикуется.
    #[test]
    fn oversize_single_chunk_error_before_copy() {
        let cancel = AtomicBool::new(false);
        let r = run_srt_fixed(
            "big1",
            "1\n00:00:00,000 --> 00:00:05,000\nМного.\n",
            &opts_with_limit(1000),
            24_000,
            &cancel,
        );
        let err = format!("{:#}", r.expect_err("ошибка лимита"));
        assert!(
            err.contains("требуется 24000 сэмплов, разрешено 1000"),
            "сообщение: {err}"
        );
    }

    /// SRT.9A-4: превышение накапливается несколькими чанками (паузы 0) —
    /// ошибка на чанке, переходящем границу.
    #[test]
    fn limit_exceeded_after_several_chunks() {
        let cancel = AtomicBool::new(false);
        // Длинная реплика режется на много чанков по max_tokens=50.
        let long_text = "Длинная реплика с несколькими частями. ".repeat(30);
        let srt = format!("1\n00:00:00,000 --> 00:10:00,000\n{long_text}\n");
        let opts = SrtOptions {
            internal_pause_ms: 0,
            comma_pause_ms: 0,
            max_cue_pcm_samples: 2_000,
            ..Default::default()
        };
        let r = run_srt_fixed("multi", &srt, &opts, 600, &cancel);
        let err = format!("{:#}", r.expect_err("ошибка накопления"));
        assert!(
            err.contains("превышает ограничение PCM-буфера"),
            "сообщение: {err}"
        );
        assert!(err.contains("разрешено 2000"), "сообщение: {err}");
    }

    /// SRT.9A-5: PCM помещается, но внутренняя пауза превышает лимит —
    /// ошибка ДО resize буфера.
    #[test]
    fn internal_pause_over_limit_error_before_resize() {
        let cancel = AtomicBool::new(false);
        let long_text = "Длинная реплика с несколькими частями. ".repeat(30);
        let srt = format!("1\n00:00:00,000 --> 00:10:00,000\n{long_text}\n");
        // чанк 600, после первого чанка пауза 500 мс = 12000 сэмплов:
        // 600 + 12000 > 2000 → ошибка до resize.
        let opts = SrtOptions {
            internal_pause_ms: 500,
            comma_pause_ms: 500,
            max_cue_pcm_samples: 2_000,
            ..Default::default()
        };
        let r = run_srt_fixed("pause", &srt, &opts, 600, &cancel);
        let err = format!("{:#}", r.expect_err("ошибка паузы"));
        assert!(
            err.contains("требуется 12600 сэмплов, разрешено 2000"),
            "сообщение: {err}"
        );
    }

    /// SRT.9A-6: граничные значения u64/usize — без паники и переполнений.
    #[test]
    fn capacity_check_arithmetic_boundaries() {
        // обычное превышение — ошибка с требуемым/разрешённым
        let e = ensure_cue_capacity(10, 20, 25, 7).unwrap_err();
        assert!(
            format!("{e:#}").contains("требуется 30 сэмплов, разрешено 25"),
            "{e:#}"
        );
        // точное совпадение — Ok
        assert_eq!(ensure_cue_capacity(10, 15, 25, 7).unwrap(), 25);
        // нулевая добавка на границе usize::MAX — Ok без роста
        let max = usize::MAX as u64;
        assert_eq!(
            ensure_cue_capacity(max, 0, u64::MAX, 7).unwrap(),
            usize::MAX
        );
        // переполнение u64 при сложении — явная ошибка, не паника
        let e = ensure_cue_capacity(u64::MAX, 1, u64::MAX, 7).unwrap_err();
        assert!(format!("{e:#}").contains("переполнение счётчика"), "{e:#}");
        // значение в пределах u64, но больше usize — ошибка на 32-бит;
        // на 64-бит usize совпадает с u64: максимум представим и проходит
        if usize::BITS < 64 {
            let e = ensure_cue_capacity(4_000_000_000u64, 1, u64::MAX, 7).unwrap_err();
            assert!(
                format!("{e:#}").contains("не поддерживается платформой"),
                "{e:#}"
            );
        } else {
            assert_eq!(
                ensure_cue_capacity(u64::MAX - 1, 1, u64::MAX, 7).unwrap(),
                usize::MAX
            );
        }
    }

    /// SRT.9A-8: многчанковая реплика под лимитом — пик буфера равен сумме
    /// PCM + внутренних пауз и не превышает лимит.
    #[test]
    fn multichunk_cue_peak_buffer_tracked() {
        let cancel = AtomicBool::new(false);
        let long_text = "Длинная реплика с несколькими частями. ".repeat(30);
        let srt = format!("1\n00:00:00,000 --> 00:10:00,000\n{long_text}\n");
        let limit = 1_000_000u64;
        let opts = SrtOptions {
            max_cue_pcm_samples: limit,
            ..Default::default()
        };
        let r = run_srt_fixed("peak", &srt, &opts, 24_000, &cancel)
            .expect("многчанковая реплика в лимите");
        assert_eq!(r.status, RunStatus::Completed);
        assert_eq!(r.timing_rows.len(), 1, "одна реплика");
        assert!(r.max_cue_pcm_len > 24_000, "несколько чанков собраны");
        assert!(
            (r.max_cue_pcm_len as u64) <= limit,
            "пик буфера {} <= лимита {limit}",
            r.max_cue_pcm_len
        );
        // план из 15 чанков (как в repro-прогоне): 15*24000 + 14*12000 пауз
        assert_eq!(r.max_cue_pcm_len, 15 * 24_000 + 14 * 12_000);
    }

    /// SRT.9A-7: пустой PCM чанка — реплика завершается без паники.
    #[test]
    fn empty_pcm_chunk_completes() {
        let cancel = AtomicBool::new(false);
        let r = run_srt_fixed(
            "empty",
            "1\n00:00:00,000 --> 00:00:02,000\nТихо.\n",
            &SrtOptions::default(),
            0,
            &cancel,
        )
        .unwrap();
        assert_eq!(r.status, RunStatus::Completed);
        assert_eq!(r.timing_rows.len(), 1);
        assert_eq!(r.timing_rows[0].raw_samples, 0);
    }

    #[test]
    fn timing_log_written_beside_result() {
        let st = load_st();
        let cancel = AtomicBool::new(false);
        let out = temp_wav("log");
        let sink = make_sink(&out, 24_000, OutputFormat::Wav, "", None).unwrap();
        let srt = "1\n00:00:00,000 --> 00:00:00,500\nЛог.\n";
        let r = run_srt(srt, sink, &SrtOptions::default(), st.as_ref(), &cancel).unwrap();
        let published = r.path.clone().unwrap();
        let log = write_srt_timing_log(&published, "test.srt", &r).unwrap();
        assert!(log.exists());
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.contains("Лог таймингов SRT"));
        assert!(text.contains("Исходник: test.srt"));
        assert!(text.contains("[1]"));
    }

    /// ТЗ §12.D: MP3 и Opus кодируются ОДНИМ FFmpeg на файл (без
    /// перезапуска на реплику). Требует ffmpeg в окружении — при
    /// отсутствии пропускается с громкой диагностикой.
    #[test]
    fn mp3_opus_single_ffmpeg_each() {
        let Some(ffmpeg) = crate::sink::resolve_ffmpeg(None).ok() else {
            eprintln!("SKIP: ffmpeg не найден в окружении теста");
            return;
        };
        let st = load_st();
        for (fmt, ext, preset) in [
            (OutputFormat::Mp3, "mp3", "-V2"),
            (OutputFormat::Opus, "opus", "opus 96k"),
        ] {
            let cancel = AtomicBool::new(false);
            let dir = std::env::temp_dir().join("srt4_tests").join(format!(
                "ff_{}_{}",
                ext,
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let mut out = dir.join(format!("out.{ext}"));
            let mut n = 0u32;
            while out.exists() {
                n += 1;
                out = dir.join(format!("out_{n}.{ext}"));
            }
            let sink = make_sink(&out, 24_000, fmt, preset, Some(&ffmpeg)).unwrap();
            let srt = "1\n00:00:00,000 --> 00:00:01,200\nПервая реплика файла.\n\n2\n00:00:02,000 --> 00:00:03,500\nВторая реплика файла.\n";
            let r = run_srt(srt, sink, &SrtOptions::default(), st.as_ref(), &cancel)
                .unwrap_or_else(|e| panic!("{fmt:?}: {e:#}"));
            assert_eq!(r.status, RunStatus::Completed, "{fmt:?}");
            let published = r.path.unwrap();
            assert!(published.exists(), "{fmt:?}: {}", published.display());
            let size = std::fs::metadata(&published).map(|m| m.len()).unwrap_or(0);
            assert!(size > 1000, "{fmt:?}: размер {size}");
        }
    }
}

//! SRT-конвейер: планировщик таймингов (этап SRT.2) и потоковая сборка
//! реплик (SRT.4/SRT.5). Контракт — docs/SRT_0_AUDIT_CONTRACT.md §8–9.
//!
//! Планировщик — дословный перенос `_assemble_srt_track` Python-эталона
//! (`pocket-tts-srt_gui.py`) на целочисленные позиции сэмплов: все позиции
//! u64, внутренняя арифметика f64 (как Python float), частота 24 000 Гц
//! даёт точную конвертацию мс ↔ сэмплы (1 мс = 24 сэмпла) — накопления
//! ошибки округления нет на многочасовых дорожках.
//!
//! Алгоритм на реплику (см. контракт §8): тишина до `start` при опережении
//! больше 20 мс; допустимая длительность `target + borrow` (заимствование
//! у паузы до следующей реплики списка, не короче `min_pause_ms`); жёсткий
//! лаг при `allowable` меньше 100 мс — потолок `raw / max_tempo` (quirk
//! Python сохранён); ускорение только когда `raw` больше `allowable`,
//! темп ограничен диапазоном `(1.0, max_tempo]`, замедление отсутствует;
//! позиция трека после реплики — фактическое число сэмплов (дрейфа нет).

/// Параметры планировщика (значения GUI). `sample_rate` фиксируется
/// движком PocketTTS (24 000); поле оставлено для явности контракта.
#[derive(Debug, Clone, PartialEq)]
pub struct SrtTimingParams {
    pub sample_rate: u32,
    /// Потолок ускорения ×1.1..=×3.0 (дефолт ×2.0).
    pub max_tempo: f32,
    /// Минимальная тишина между репликами при заимствовании, мс 0..=500
    /// (дефолт 100).
    pub min_pause_ms: u32,
}

impl Default for SrtTimingParams {
    fn default() -> Self {
        Self {
            sample_rate: 24_000,
            max_tempo: 2.0,
            min_pause_ms: 100,
        }
    }
}

impl SrtTimingParams {
    /// Мс → сэмплы (точное умножение при 24 кГц; общий случай — округление
    /// к ближайшему, единая политика конвертации контракта §8).
    pub fn ms_to_samples(&self, ms: u64) -> u64 {
        (ms as f64 * self.sample_rate as f64 / 1000.0).round() as u64
    }

    /// Сэмплы → мс (диагностика; при 24 кГц точное деление на 24).
    pub fn samples_to_ms(&self, samples: u64) -> f64 {
        samples as f64 * 1000.0 / self.sample_rate as f64
    }

    fn min_pause_samples(&self) -> u64 {
        self.ms_to_samples(self.min_pause_ms as u64)
    }
}

/// Порог вставки тишины до реплики: 20 мс (Python `pause_needed_ms > 20`).
pub const LEAD_SILENCE_THRESHOLD_MS: u64 = 20;
/// Порог жёсткого лага: допустимая длительность < 100 мс (Python).
pub const HARD_LAG_ALLOWABLE_MS: u64 = 100;

/// Решение планировщика по одной реплике (до применения SoundTouch):
/// сколько тишины вставить перед репликой и какой темп применить.
#[derive(Debug, Clone, PartialEq)]
pub struct CuePlan {
    /// Тишина перед репликой в сэмплах (0 — трек уже не опережает
    /// расписание или отставание ≤ 20 мс).
    pub silence_before: u64,
    /// Темп ускорения; `None` — реплика влезает, исходный PCM без
    /// обработки SoundTouch.
    pub tempo: Option<f32>,
    /// Допустимая длительность без ускорения (для диагностики), сэмплы.
    pub allowable: u64,
    /// Заимствовано у паузы до следующей реплики, сэмплы.
    pub borrow: u64,
    /// Сработал жёсткий лаг (окно < 100 мс): потолок вместо окна.
    pub hard_lag: bool,
    /// Целевая длительность ускорения (диагностика), сэмплы.
    pub target_duration: u64,
}

/// Планировщик с позицией трека. Одна дорожка (один SRT-файл); реплики
/// без чанков не планируются вовсе (окно поглощается тишиной следующей
/// озвучиваемой реплики) — как Python.
#[derive(Debug, Clone)]
pub struct SrtTimingPlanner {
    params: SrtTimingParams,
    /// Текущая позиция готового трека в сэмплах (u64 — контракт §8).
    track: u64,
}

impl SrtTimingPlanner {
    pub fn new(params: SrtTimingParams) -> Self {
        Self { params, track: 0 }
    }

    pub fn params(&self) -> &SrtTimingParams {
        &self.params
    }

    /// Позиция трека (сэмплы).
    pub fn track(&self) -> u64 {
        self.track
    }

    /// Шаги 1–4 алгоритма для озвучиваемой реплики. `next_start` — начало
    /// следующей реплики СПИСКА (включая пустые без чанков — их пауза
    /// тоже можно занимать, как в Python); `None` для последней реплики.
    ///
    /// Планирование вставляет тишину в позицию трека немедленно (как
    /// Python обновляет `current_track_time_ms` при вставке); фактическая
    /// длительность реплики фиксируется отдельным `commit`.
    pub fn plan_cue(
        &mut self,
        start: u64,
        end: u64,
        raw_samples: u64,
        next_start: Option<u64>,
    ) -> CuePlan {
        // Шаг 1: опережаем расписание — тишина до начала реплики.
        let pause_needed = start.saturating_sub(self.track);
        let threshold = self.params.ms_to_samples(LEAD_SILENCE_THRESHOLD_MS);
        let silence_before = if pause_needed > threshold {
            self.track += pause_needed;
            pause_needed
        } else {
            0
        };

        // Шаг 2: доступная длительность до конца реплики от новой позиции.
        let target = end.saturating_sub(self.track);

        // Шаг 3: заимствование у паузы до следующей реплики.
        let min_pause = self.params.min_pause_samples();
        let borrow = next_start
            .map(|ns| ns.saturating_sub(end).saturating_sub(min_pause))
            .unwrap_or(0);
        let mut allowable = target.saturating_add(borrow);

        // Жёсткий лаг: времени < 100 мс — разрешаем минимум (потолок
        // ускорения); quirk Python: влезающая реплика тоже ускоряется.
        let hard_lag_threshold = self.params.ms_to_samples(HARD_LAG_ALLOWABLE_MS);
        let mut hard_lag = false;
        if allowable < hard_lag_threshold {
            allowable = (raw_samples as f64 / self.params.max_tempo as f64) as u64;
            hard_lag = true;
        }

        // Шаг 4: решение об ускорении.
        let (tempo, target_duration) = if raw_samples > allowable {
            let max_tempo = self.params.max_tempo as f64;
            let min_possible = (raw_samples as f64 / max_tempo) as u64;
            let target_dur = allowable.max(min_possible);
            let ratio = raw_samples as f64 / target_dur as f64;
            // 1.0 < tempo <= max_tempo; f64-математика и потолок target_dur
            // гарантируют границы, clamp — страховка от округления f32.
            let clamped = ratio.clamp(1.0, max_tempo);
            let t = if clamped > 1.0 + 1e-9 {
                Some(clamped as f32)
            } else {
                None
            };
            (t, target_dur)
        } else {
            (None, raw_samples)
        };

        CuePlan {
            silence_before,
            tempo,
            allowable,
            borrow,
            hard_lag,
            target_duration,
        }
    }

    /// Шаг 5: зафиксировать фактическую длительность реплики (после
    /// SoundTouch — по реальному числу полученных сэмплов, включая
    /// flush-хвост; без ускорения — `raw_samples`).
    pub fn commit(&mut self, actual_samples: u64) {
        self.track += actual_samples;
    }

    /// Диагностика реплики для лога таймингов (контракт §8.1): план +
    /// фактические начало/конец/опоздание. `actual_samples` — реально
    /// записанная длительность (== target планировщика не обязана).
    pub fn cue_diagnostics(
        &self,
        cue_index: i64,
        start: u64,
        end: u64,
        raw_samples: u64,
        plan: &CuePlan,
        actual_samples: u64,
    ) -> CueDiagnostics {
        let speech_start = self.track.saturating_sub(actual_samples);
        let actual_end = self.track;
        CueDiagnostics {
            cue_index,
            start_samples: start,
            end_samples: end,
            speech_start_samples: speech_start,
            raw_samples,
            allowable_samples: plan.allowable,
            borrow_samples: plan.borrow,
            tempo: plan.tempo,
            final_samples: actual_samples,
            actual_end_samples: actual_end,
            late_by_samples: actual_end.saturating_sub(end),
            hard_lag: plan.hard_lag,
            sample_rate: self.params.sample_rate,
        }
    }
}

/// Строка диагностики таймингов одной реплики (все позиции — сэмплы;
/// сериализация в мс — `to_ms_*`/лог SRT.4).
#[derive(Debug, Clone, PartialEq)]
pub struct CueDiagnostics {
    pub cue_index: i64,
    pub start_samples: u64,
    pub end_samples: u64,
    /// Фактическое начало речи реплики в треке.
    pub speech_start_samples: u64,
    /// Длительность речи после синтеза и обрезки (до ускорения).
    pub raw_samples: u64,
    /// Допустимая длительность (окно + заимствование; при жёстком лаге —
    /// пересчитанный потолок).
    pub allowable_samples: u64,
    pub borrow_samples: u64,
    pub tempo: Option<f32>,
    /// Итоговая длительность (фактическая, после SoundTouch).
    pub final_samples: u64,
    /// Фактический конец реплики в треке.
    pub actual_end_samples: u64,
    /// Опоздание относительно заданного конца (0 — успели).
    pub late_by_samples: u64,
    pub hard_lag: bool,
    pub sample_rate: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: u32 = 24_000;

    fn ms(ms: u64) -> u64 {
        ms * SR as u64 / 1000
    }

    fn params() -> SrtTimingParams {
        SrtTimingParams::default()
    }

    #[test]
    fn ms_samples_conversion_exact_at_24k() {
        let p = params();
        assert_eq!(p.ms_to_samples(1), 24);
        assert_eq!(p.ms_to_samples(100), 2400);
        assert_eq!(p.ms_to_samples(0), 0);
        assert_eq!(p.samples_to_ms(24), 1.0);
        // многочасовая дорожка: 3 часа = 259_200_000 мс без потерь
        let big = 3 * 60 * 60 * 1000;
        assert_eq!(p.ms_to_samples(big), 259_200_000);
        assert_eq!(p.samples_to_ms(259_200_000), big as f64);
    }

    /// ТЗ §12.B.1: реплика короче окна → без ускорения.
    #[test]
    fn fits_window_no_tempo() {
        let mut pl = SrtTimingPlanner::new(params());
        let plan = pl.plan_cue(ms(0), ms(3000), ms(1500), Some(ms(4000)));
        assert_eq!(plan.silence_before, 0);
        assert_eq!(plan.tempo, None);
        assert_eq!(plan.allowable, ms(3000) + ms(900));
    }

    /// ТЗ §12.B.1 (продолжение): старт позже нуля.
    #[test]
    fn lead_silence_inserted_when_ahead() {
        let mut pl = SrtTimingPlanner::new(params());
        let plan = pl.plan_cue(ms(1000), ms(3000), ms(1500), Some(ms(4000)));
        assert_eq!(plan.silence_before, ms(1000));
        assert_eq!(pl.track(), ms(1000));
        assert_eq!(plan.tempo, None);
        assert_eq!(plan.borrow, ms(4000) - ms(3000) - ms(100));
        pl.commit(ms(1500));
        assert_eq!(pl.track(), ms(2500));
    }

    /// ТЗ §12.B.2: длительность точно равна доступному времени.
    #[test]
    fn exact_fit_no_tempo() {
        let mut pl = SrtTimingPlanner::new(params());
        // окно 1000..3000, трек 0 → тишина 1000, окно 2000, raw 2000
        let plan = pl.plan_cue(ms(1000), ms(3000), ms(2000), Some(ms(3500)));
        assert_eq!(plan.silence_before, ms(1000));
        assert_eq!(plan.allowable, ms(2000) + (ms(500) - ms(100)));
        assert_eq!(plan.tempo, None);
    }

    /// ТЗ §12.B.3-4: заимствование из зазора перед следующей репликой.
    #[test]
    fn borrowing_from_next_gap() {
        let mut pl = SrtTimingPlanner::new(params());
        // окно 2000, raw 2400, зазор до следующей 1000, min_pause 100 →
        // borrow 900, allowable 2900 ≥ raw → без ускорения.
        let plan = pl.plan_cue(ms(1000), ms(3000), ms(2400), Some(ms(4000)));
        assert_eq!(plan.borrow, ms(900));
        assert_eq!(plan.tempo, None);
    }

    /// ТЗ §12.B.5: заимствование не превышает резерв (зазор мал).
    #[test]
    fn borrowing_capped_by_gap() {
        let mut pl = SrtTimingPlanner::new(params());
        // зазор 200 < min_pause 100... зазор 200, резерв = 100
        let plan = pl.plan_cue(ms(1000), ms(3000), ms(2400), Some(ms(3200)));
        assert_eq!(plan.borrow, ms(100));
        // окно 2000 + 100 < raw 2400 → tempo = 2400/2100 ≈ 1.143
        let t = plan.tempo.expect("tempo expected");
        assert!((t - 2400.0 / 2100.0).abs() < 1e-4, "tempo {t}");
        assert!(t > 1.0 && t <= 2.0);
    }

    /// ТЗ §12.B.6: нужен tempo 1.25.
    #[test]
    fn tempo_125() {
        let mut pl = SrtTimingPlanner::new(params());
        // окно 1600, raw 2000, зазора нет (next сразу) → 2000/1600 = 1.25
        let plan = pl.plan_cue(ms(0), ms(1600), ms(2000), Some(ms(1600)));
        assert_eq!(plan.borrow, 0);
        let t = plan.tempo.expect("tempo");
        assert!((t - 1.25).abs() < 1e-5, "tempo {t}");
        assert_eq!(plan.target_duration, ms(1600));
    }

    /// ТЗ §12.B.7: нужен tempo 2.0 (потолок).
    #[test]
    fn tempo_20() {
        let mut pl = SrtTimingPlanner::new(params());
        let plan = pl.plan_cue(ms(0), ms(1000), ms(2000), Some(ms(1000)));
        let t = plan.tempo.expect("tempo");
        assert!((t - 2.0).abs() < 1e-5, "tempo {t}");
    }

    /// ТЗ §12.B.8: требуется темп выше максимума → ограничение потолком.
    #[test]
    fn tempo_capped_at_max() {
        let mut pl = SrtTimingPlanner::new(params());
        // raw 6000 в окно 1000: минимум 3000, tempo = 2.0, реплика опоздает
        let plan = pl.plan_cue(ms(0), ms(1000), ms(6000), Some(ms(1000)));
        let t = plan.tempo.expect("tempo");
        assert!((t - 2.0).abs() < 1e-5, "tempo {t}");
        assert_eq!(plan.target_duration, ms(3000));
        // слова не обрезаются: итоговая длительность = raw/tempo = 3000
    }

    /// ТЗ §12.B.9: соседние реплики перекрываются → без тишины и займа,
    /// жёсткий лаг сжимает по потолку.
    #[test]
    fn overlapping_cues_hard_lag() {
        let mut pl = SrtTimingPlanner::new(params());
        pl.commit(ms(5000)); // трек уже глубоко в чужом окне
        let plan = pl.plan_cue(ms(1000), ms(6000), ms(3000), Some(ms(6500)));
        // pause_needed = 1000-5000 < 0 → без тишины; target = 6000-5000 = 1000
        assert_eq!(plan.silence_before, 0);
        // borrow = 6500-6000-100 = 400 → allowable = 1400 ≥ 100 → не лаг
        assert!(!plan.hard_lag);
        assert_eq!(plan.allowable, ms(1400));
        // raw 3000 > 1400 → tempo = 3000/1500 = 2.0
        let t = plan.tempo.expect("tempo");
        assert!((t - 2.0).abs() < 1e-5);
    }

    /// Жёсткий лаг: допустимое окно < 100 мс → потолок raw/max_tempo
    /// (quirk Python: даже влезающая реплика ускоряется до максимума).
    #[test]
    fn hard_lag_quirk_compresses_fitting_cue() {
        let mut pl = SrtTimingPlanner::new(params());
        // окно 90 мс, raw 50 мс (влезает!), next сразу после конца.
        // allowable(90+0) < 100 → allowable = 50/2 = 25 мс → raw > 25 →
        // tempo = 50/25 = 2.0 — реплика сжата, quirk сохранён.
        let plan = pl.plan_cue(ms(0), ms(90), ms(50), Some(ms(90)));
        assert!(plan.hard_lag);
        let t = plan.tempo.expect("tempo");
        assert!((t - 2.0).abs() < 1e-4, "tempo {t}");
    }

    /// ТЗ §12.B.10: синтез отстаёт от таймкодов — опоздание честно
    /// фиксируется, темп на потолке, слова не обрезаются.
    #[test]
    fn late_synthesis_reported() {
        let mut pl = SrtTimingPlanner::new(params());
        let plan = pl.plan_cue(ms(0), ms(1000), ms(5000), Some(ms(1000)));
        // потолок: target = 5000/2 = 2500 мс, tempo 2.0
        pl.commit(plan.target_duration);
        let d = pl.cue_diagnostics(1, ms(0), ms(1000), ms(5000), &plan, plan.target_duration);
        assert_eq!(d.late_by_samples, ms(1500)); // конец 2500 против end 1000
    }

    /// ТЗ §12.B.11: пустая реплика между двумя озвучиваемыми — её пауза
    /// доступна для заимствования (next — по СПИСКУ, включая пустые).
    #[test]
    fn empty_cue_gap_is_borrowable() {
        let mut pl = SrtTimingPlanner::new(params());
        // текущая 0..2000; следующая в списке ПУСТАЯ 2100..2500, за ней
        // озвучиваемая 3000: next_start = 2100 (как в Python).
        let plan = pl.plan_cue(ms(0), ms(2000), ms(2600), Some(ms(2100)));
        // зазор 100 < min_pause → borrow 0; окно 2000 → tempo = 2600/2000
        assert_eq!(plan.borrow, 0);
        assert_eq!(plan.allowable, ms(2000));
        let t = plan.tempo.expect("tempo");
        assert!((t - 1.3).abs() < 1e-5, "tempo {t}");
    }

    /// ТЗ §12.B.12: последняя реплика — заимствования нет.
    #[test]
    fn last_cue_no_borrow() {
        let mut pl = SrtTimingPlanner::new(params());
        let plan = pl.plan_cue(ms(0), ms(2000), ms(3900), None);
        assert_eq!(plan.borrow, 0);
        let t = plan.tempo.expect("tempo");
        // 3900/2000 = 1.95 < потолка 2.0
        assert!((t - 1.95).abs() < 1e-5, "tempo {t}");
    }

    /// ТЗ §12.B.13: многочасовой SRT — позиции u64, конвертация мс↔сэмплы
    /// точная, дрейфа нет (контракт §8).
    #[test]
    fn multi_hour_track_no_drift() {
        let p = params();
        let mut pl = SrtTimingPlanner::new(p.clone());
        // 500 реплик по ~7 с с паузами — 58 минут; затем контроль позиций.
        let mut expected_track = 0u64;
        for i in 0..500u64 {
            let start = ms(i * 8000 + 500);
            let end = ms(i * 8000 + 4500);
            let next = Some(ms((i + 1) * 8000 + 500));
            let raw = ms(3800);
            let plan = pl.plan_cue(start, end, raw, next);
            assert_eq!(plan.tempo, None, "cue {i}");
            let actual = plan.silence_before + raw;
            pl.commit(raw);
            expected_track += actual;
            assert_eq!(pl.track(), expected_track, "cue {i}");
        }
        // каждая реплика начинается ровно на своей позиции (тишина точная);
        // итоговая позиция — точно конец последней реплики, без дрейфа
        let expected_final = ms(499 * 8000 + 500 + 3800);
        assert_eq!(pl.track(), expected_final);
        assert_eq!(pl.track(), ms(3_996_300));
    }

    /// Отмена невозможности: max_tempo = 1.0 → ускорения нет никогда.
    #[test]
    fn max_tempo_one_never_accelerates() {
        let mut pl = SrtTimingPlanner::new(SrtTimingParams {
            max_tempo: 1.0,
            ..Default::default()
        });
        let plan = pl.plan_cue(ms(0), ms(1000), ms(3000), Some(ms(1000)));
        assert_eq!(plan.tempo, None);
        assert_eq!(plan.target_duration, ms(3000));
    }

    /// Диагностика: поля начала/конца/опоздания.
    #[test]
    fn diagnostics_fields() {
        let mut pl = SrtTimingPlanner::new(params());
        let plan = pl.plan_cue(ms(1000), ms(3000), ms(2000), Some(ms(4000)));
        pl.commit(ms(2000));
        let d = pl.cue_diagnostics(7, ms(1000), ms(3000), ms(2000), &plan, ms(2000));
        assert_eq!(d.cue_index, 7);
        assert_eq!(d.speech_start_samples, ms(1000));
        assert_eq!(d.actual_end_samples, ms(3000));
        assert_eq!(d.late_by_samples, 0);
        assert_eq!(d.tempo, None);
        assert_eq!(d.final_samples, ms(2000));
    }

    /// Python-семантика «вставляется вся разность»: тишина 1024 мс при
    /// паузе 1024 мс (не 1024-20).
    #[test]
    fn silence_is_full_gap_not_minus_threshold() {
        let mut pl = SrtTimingPlanner::new(params());
        let plan = pl.plan_cue(ms(1024), ms(2000), ms(900), Some(ms(3000)));
        assert_eq!(plan.silence_before, ms(1024));
    }

    /// Гэп ровно 20 мс — порог НЕ срабатывает (Python `> 20`).
    #[test]
    fn silence_threshold_strictly_greater() {
        let mut pl = SrtTimingPlanner::new(params());
        pl.commit(ms(980));
        let plan = pl.plan_cue(ms(1000), ms(2000), ms(900), Some(ms(3000)));
        assert_eq!(plan.silence_before, 0);
        let mut pl2 = SrtTimingPlanner::new(params());
        pl2.commit(ms(979));
        let plan2 = pl2.plan_cue(ms(1000), ms(2000), ms(900), Some(ms(3000)));
        assert_eq!(plan2.silence_before, ms(21));
    }
}

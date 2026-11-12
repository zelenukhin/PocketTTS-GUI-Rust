//! Настройки синтеза. Имена полей и ограничения (clamps) зеркалируют
//! pocket-tts_gui.py (`save_settings`/`load_settings`/`_collect_settings`),
//! чтобы будущий GUI мог читать существующий settings_pocket_tts.json.
//! Поля GUI, относящиеся к ещё не портированным подсистемам (чанкование,
//! паузы, ffmpeg), сюда добавляются по мере переноса — заранее их нет.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SynthSettings {
    /// Температура шума (GUI: слайдер 0.1–1.5, шаг 0.05).
    pub temperature: f32,
    /// Шаги декодирования flow-головы (GUI: слайдер 1–32; модель дистиллирована под 1).
    pub sampler_decode_steps: u32,
    /// Порог EOS (GUI: «Авто»/список; движок Shtorm: тератт-семантика −4).
    pub eos_threshold: f32,
    /// Seed; −1 — случайный на каждый запуск (GUI: DEFAULT_SEED = 42).
    pub seed: i64,
    /// «Яркость», дБ high-shelf выше 3 кГц; 0 — выкл (GUI: 0–12, дефолт 8).
    pub brightness_db: f32,
    /// «Очистка звука (нейросеть)» DPDFNet.
    pub denoise: bool,
    /// «Защита от повторов teratts-android» (двухпопытная схема движка).
    pub anti_repeat: bool,
    /// Масштаб пауз (GUI: 0–2).
    pub pause_scale: f32,
    /// Пауза абзаца, с (GUI: 0–3).
    pub paragraph_pause_s: f32,
    /// Устройство: "Auto" | "CPU" | "CUDA" (как в GUI; резолв — в app-core).
    pub device: String,
    /// Число процессов/инстансов синтеза (GUI: 1–12).
    pub num_model_instances: u32,
}

impl Default for SynthSettings {
    fn default() -> Self {
        // Дефолты — как в каркасе GUI (settings_pocket_tts.json пользователя
        // может отличаться; загрузка из файла не подменяет их молча).
        Self {
            temperature: 0.3,
            sampler_decode_steps: 1,
            eos_threshold: -4.0,
            seed: 42,
            brightness_db: 8.0,
            denoise: false,
            anti_repeat: true,
            pause_scale: 1.0,
            paragraph_pause_s: 0.9,
            device: "Auto".into(),
            num_model_instances: 1,
        }
    }
}

impl SynthSettings {
    /// Приводит значения в допустимые границы (как load_settings GUI).
    pub fn clamped(mut self) -> Self {
        self.temperature = self.temperature.clamp(0.05, 1.5);
        self.sampler_decode_steps = self.sampler_decode_steps.clamp(1, 32);
        self.brightness_db = self.brightness_db.clamp(0.0, 12.0);
        self.pause_scale = self.pause_scale.clamp(0.0, 2.0);
        self.paragraph_pause_s = self.paragraph_pause_s.clamp(0.0, 3.0);
        self.num_model_instances = self.num_model_instances.clamp(1, 12);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_gui() {
        let s = SynthSettings::default();
        assert_eq!(s.seed, 42);
        assert!((s.eos_threshold - (-4.0)).abs() < 1e-6); // TERATTS_EOS_THRESHOLD
        assert_eq!(s.sampler_decode_steps, 1);
        assert!((s.temperature - 0.3).abs() < 1e-6);
        assert!((s.brightness_db - 8.0).abs() < 1e-6);
        assert!(s.anti_repeat);
    }

    #[test]
    fn clamps_out_of_range() {
        let s = SynthSettings {
            temperature: 9.0,
            brightness_db: 99.0,
            pause_scale: -5.0,
            num_model_instances: 99,
            ..Default::default()
        }
        .clamped();
        assert_eq!(s.temperature, 1.5);
        assert_eq!(s.brightness_db, 12.0);
        assert_eq!(s.pause_scale, 0.0);
        assert_eq!(s.num_model_instances, 12);
    }

    #[test]
    fn reads_gui_field_names() {
        // Фрагмент реального settings_pocket_tts.json
        let raw = r#"{
            "temperature": 0.3, "sampler_decode_steps": 1,
            "eos_teratts": true, "anti_repeat_teratts": true,
            "denoise": false, "brightness_db": 8, "seed": 42,
            "device": "Auto", "num_model_instances": 3
        }"#;
        let v: serde_json::Value = serde_json::from_str(raw).unwrap();
        let s = SynthSettings {
            temperature: v["temperature"].as_f64().unwrap() as f32,
            sampler_decode_steps: v["sampler_decode_steps"].as_u64().unwrap() as u32,
            seed: v["seed"].as_i64().unwrap(),
            brightness_db: v["brightness_db"].as_f64().unwrap() as f32,
            denoise: v["denoise"].as_bool().unwrap(),
            anti_repeat: v["anti_repeat_teratts"].as_bool().unwrap(),
            ..Default::default()
        };
        assert_eq!(s.num_model_instances, 1); // GUI-имя замаплено отдельно
        assert_eq!(s.seed, 42);
    }
}

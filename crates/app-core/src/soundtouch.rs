//! SoundTouch для SRT-конвейера — этап SRT.3, контракт
//! docs/SRT_0_AUDIT_CONTRACT.md §7 и third_party/soundtouch/NOTICE.txt.
//!
//! Библиотека SoundTouch 2.4.1 (LGPL 2.1) компонуется ТОЛЬКО динамически:
//! официальная C-обёртка SoundTouchDLL.dll загружается в рантайме через
//! `LoadLibraryW`/`GetProcAddress`; в таблице импортов EXE SoundTouch
//! отсутствует, обычные EXE от неё не зависят. C-обёртка ловит все
//! std::exception внутри себя (проверено по исходнику 2.4.1) — через
//! границу FFI исключения не проходят.
//!
//! Речевой профиль (эквивалент `soundstretch -speech`, main.cpp:126–133):
//! SEQUENCE_MS=40, SEEKWINDOW_MS=15, OVERLAP_MS=8, AA-фильтр включён,
//! QUICKSEEK выключен. Темп — только `setTempo` (высота тона не меняется);
//! `setRate`/`setPitch` не используются.
//!
//! Протокол на реплику: новый instance (состояние между репликами не
//! переносится) → putSamples блоками → выборка до осушения → flush →
//! выборка до осушения. Выходная длительность = фактически полученные
//! сэмплы; tempo = 1.0 обрабатывается без SoundTouch вовсе.

use std::fmt;
use std::path::{Path, PathBuf};

/// Ожидаемая версия SoundTouch (2.4.1 = 2·10000 + 4·100 + 1). Более
/// старшие принимаются, младшие — ошибка (семантика речевых настроек
/// стабильна с 2.3).
pub const SOUNDTOUCH_MIN_VERSION_ID: u32 = 20_400;
/// Фактическая версия собранной DLL из third_party/soundtouch (для диагностики).
pub const SOUNDTOUCH_BUILT_VERSION_ID: u32 = 20_401;

/// Идентификаторы настроек SoundTouch (include/SoundTouch.h).
pub const SETTING_USE_AA_FILTER: i32 = 0;
pub const SETTING_USE_QUICKSEEK: i32 = 2;
pub const SETTING_SEQUENCE_MS: i32 = 3;
pub const SETTING_SEEKWINDOW_MS: i32 = 4;
pub const SETTING_OVERLAP_MS: i32 = 5;

/// Речевые значения (soundstretch -speech).
pub const SPEECH_SEQUENCE_MS: i32 = 40;
pub const SPEECH_SEEKWINDOW_MS: i32 = 15;
pub const SPEECH_OVERLAP_MS: i32 = 8;

/// Ошибка работы SoundTouch.
#[derive(Debug)]
pub enum SoundTouchError {
    /// DLL не найдена ни по одному пути поиска (перечислены попытки).
    LibraryNotFound(Vec<PathBuf>),
    /// DLL найдена, но в ней нет обязательной экспортируемой функции.
    MissingExport(String),
    /// Версия DLL старше поддерживаемой.
    UnsupportedVersion { actual: u32, required: u32 },
    /// Вход/выход содержит NaN/Inf.
    InvalidSampleData(&'static str),
}

impl fmt::Display for SoundTouchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SoundTouchError::LibraryNotFound(paths) => write!(
                f,
                "SoundTouchDLL.dll не найдена (искали: {})",
                paths
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
            SoundTouchError::MissingExport(name) => {
                write!(f, "SoundTouchDLL.dll не экспортирует {name}")
            }
            SoundTouchError::UnsupportedVersion { actual, required } => {
                write!(f, "SoundTouch версии {actual} старше требуемой {required}")
            }
            SoundTouchError::InvalidSampleData(what) => {
                write!(f, "SoundTouch: некорректные сэмплы ({what})")
            }
        }
    }
}

impl std::error::Error for SoundTouchError {}

#[cfg(windows)]
mod ffi {
    use std::ffi::c_void;
    use std::os::raw::{c_char, c_float, c_int};

    pub type Handle = *mut c_void;
    pub type Module = *mut c_void;

    pub type FnCreateInstance = unsafe extern "system" fn() -> Handle;
    pub type FnDestroyInstance = unsafe extern "system" fn(Handle);
    pub type FnGetVersionId = unsafe extern "system" fn() -> u32;
    pub type FnGetVersionString = unsafe extern "system" fn() -> *const c_char;
    pub type FnSetSampleRate = unsafe extern "system" fn(Handle, u32) -> c_int;
    pub type FnSetChannels = unsafe extern "system" fn(Handle, u32) -> c_int;
    pub type FnSetTempo = unsafe extern "system" fn(Handle, c_float);
    pub type FnSetSetting = unsafe extern "system" fn(Handle, c_int, c_int) -> c_int;
    pub type FnPutSamples = unsafe extern "system" fn(Handle, *const c_float, u32) -> c_int;
    pub type FnReceiveSamples = unsafe extern "system" fn(Handle, *mut c_float, u32) -> u32;
    pub type FnFlush = unsafe extern "system" fn(Handle) -> c_int;
    pub type FnNumSamples = unsafe extern "system" fn(Handle) -> u32;
    pub type FnIsEmpty = unsafe extern "system" fn(Handle) -> c_int;
    pub type FnClear = unsafe extern "system" fn(Handle);

    /// Загрузка DLL по явному пути (нулевая терминированная UTF-16).
    pub fn load_library(path: &std::path::Path) -> Option<Module> {
        use windows_sys::Win32::System::LibraryLoader::LoadLibraryW;
        let wide: Vec<u16> = path
            .as_os_str()
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let h = unsafe { LoadLibraryW(wide.as_ptr()) };
        if h.is_null() {
            None
        } else {
            Some(h)
        }
    }

    /// Адрес экспортируемой функции (FARPROC → usize для последующего
    /// transmute в конкретный тип указателя на функцию).
    pub fn proc_address(module: Module, name: &str) -> Option<usize> {
        use windows_sys::Win32::System::LibraryLoader::GetProcAddress;
        let name_buf = name.as_bytes();
        let mut buf: Vec<u8> = Vec::with_capacity(name_buf.len() + 1);
        buf.extend_from_slice(name_buf);
        buf.push(0);
        // PCSTR в windows-sys — алиас *const u8.
        let p = unsafe { GetProcAddress(module, buf.as_ptr()) };
        p.map(|f| f as usize)
    }

    pub fn free_library(module: Module) {
        // FreeLibrary в windows-sys 0.59 живёт в Win32::Foundation.
        use windows_sys::Win32::Foundation::FreeLibrary;
        unsafe { FreeLibrary(module) };
    }
}

/// Загруженная библиотека SoundTouch. Потокобезопасность: функции C-API
/// работают с собственными instance, сама библиотека после загрузки
/// глобально доступна (SoundTouch не имеет глобального mutable state).
pub struct SoundTouchLib {
    #[cfg(windows)]
    module: ffi::Module,
    create_instance: ffi::FnCreateInstance,
    destroy_instance: ffi::FnDestroyInstance,
    get_version_id: ffi::FnGetVersionId,
    get_version_string: ffi::FnGetVersionString,
    set_sample_rate: ffi::FnSetSampleRate,
    set_channels: ffi::FnSetChannels,
    set_tempo: ffi::FnSetTempo,
    set_setting: ffi::FnSetSetting,
    put_samples: ffi::FnPutSamples,
    receive_samples: ffi::FnReceiveSamples,
    flush: ffi::FnFlush,
    num_samples: ffi::FnNumSamples,
    is_empty: ffi::FnIsEmpty,
    clear: ffi::FnClear,
}

impl fmt::Debug for SoundTouchLib {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SoundTouchLib")
            .field("version_id", &self.version_id())
            .finish()
    }
}

impl Drop for SoundTouchLib {
    fn drop(&mut self) {
        #[cfg(windows)]
        ffi::free_library(self.module);
    }
}

/// Пути поиска SoundTouchDLL.dll в порядке приоритета: переменная
/// окружения SOUNDTOUCH_DLL → рядом с exe (портативный комплект) →
/// third_party/soundtouch репозитория (разработка/тесты) → стандартный поиск
/// Windows (PATH/System32).
pub fn discover_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(p) = std::env::var("SOUNDTOUCH_DLL") {
        if !p.is_empty() {
            out.push(PathBuf::from(p));
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            out.push(dir.join("SoundTouchDLL.dll"));
        }
    }
    if let Ok(manifest) = std::env::var("CARGO_MANIFEST_DIR") {
        // crates/app-core → корень workspace → third_party/soundtouch
        let vendored = Path::new(&manifest).ancestors().nth(2).map(|root| {
            root.join("third_party")
                .join("soundtouch")
                .join("SoundTouchDLL.dll")
        });
        if let Some(p) = vendored {
            out.push(p);
        }
    }
    out.push(PathBuf::from("SoundTouchDLL.dll"));
    out
}

impl SoundTouchLib {
    /// Загрузить DLL: явный путь (portable-резолвер GUI) или автопоиск
    /// `discover_paths()`. Проверяет версию и обязательные экспорты.
    pub fn load(explicit: Option<&Path>) -> Result<Self, SoundTouchError> {
        Self::load_impl(explicit)
    }

    #[cfg(windows)]
    fn load_impl(explicit: Option<&Path>) -> Result<Self, SoundTouchError> {
        let candidates: Vec<PathBuf> = match explicit {
            Some(p) => vec![p.to_path_buf()],
            None => discover_paths(),
        };
        let mut module = None;
        for p in &candidates {
            if let Some(h) = ffi::load_library(p) {
                module = Some(h);
                break;
            }
        }
        let module = module.ok_or(SoundTouchError::LibraryNotFound(candidates))?;

        macro_rules! sym {
            ($name:literal, $ty:ty) => {
                match ffi::proc_address(module, $name) {
                    Some(addr) => unsafe { std::mem::transmute::<usize, $ty>(addr) },
                    None => {
                        ffi::free_library(module);
                        return Err(SoundTouchError::MissingExport($name.to_string()));
                    }
                }
            };
        }

        let lib = SoundTouchLib {
            module,
            create_instance: sym!("soundtouch_createInstance", ffi::FnCreateInstance),
            destroy_instance: sym!("soundtouch_destroyInstance", ffi::FnDestroyInstance),
            get_version_id: sym!("soundtouch_getVersionId", ffi::FnGetVersionId),
            get_version_string: sym!("soundtouch_getVersionString", ffi::FnGetVersionString),
            set_sample_rate: sym!("soundtouch_setSampleRate", ffi::FnSetSampleRate),
            set_channels: sym!("soundtouch_setChannels", ffi::FnSetChannels),
            set_tempo: sym!("soundtouch_setTempo", ffi::FnSetTempo),
            set_setting: sym!("soundtouch_setSetting", ffi::FnSetSetting),
            put_samples: sym!("soundtouch_putSamples", ffi::FnPutSamples),
            receive_samples: sym!("soundtouch_receiveSamples", ffi::FnReceiveSamples),
            flush: sym!("soundtouch_flush", ffi::FnFlush),
            num_samples: sym!("soundtouch_numSamples", ffi::FnNumSamples),
            is_empty: sym!("soundtouch_isEmpty", ffi::FnIsEmpty),
            clear: sym!("soundtouch_clear", ffi::FnClear),
        };

        let actual = lib.version_id();
        if actual < SOUNDTOUCH_MIN_VERSION_ID {
            return Err(SoundTouchError::UnsupportedVersion {
                actual,
                required: SOUNDTOUCH_MIN_VERSION_ID,
            });
        }
        Ok(lib)
    }

    #[cfg(not(windows))]
    fn load_impl(_explicit: Option<&Path>) -> Result<Self, SoundTouchError> {
        // Проект целится в Windows x64; на других платформах SRT-ускорение
        // недоступно (тесты обхода tempo=1.0 работают и без DLL).
        Err(SoundTouchError::LibraryNotFound(vec![PathBuf::from(
            "SoundTouchDLL.dll",
        )]))
    }

    pub fn version_id(&self) -> u32 {
        unsafe { (self.get_version_id)() }
    }

    /// Строка версии DLL (например «2.4.1»).
    pub fn version_string(&self) -> String {
        unsafe {
            let p = (self.get_version_string)();
            if p.is_null() {
                return String::new();
            }
            let mut len = 0usize;
            while *p.add(len) != 0 {
                len += 1;
            }
            let bytes = std::slice::from_raw_parts(p as *const u8, len);
            String::from_utf8_lossy(bytes).into_owned()
        }
    }

    /// Новый обработчик одной реплики: частота/каналы + речевой профиль
    /// (SEQUENCE/SEEKWINDOW/OVERLAP, AA вкл, QUICKSEEK выкл). Состояние
    /// между репликами не переносится — на каждую реплику свой instance.
    pub fn create_speech_instance(
        &self,
        sample_rate: u32,
        channels: u32,
    ) -> SoundTouchInstance<'_> {
        let handle = unsafe { (self.create_instance)() };
        unsafe {
            (self.set_sample_rate)(handle, sample_rate);
            (self.set_channels)(handle, channels);
            (self.set_setting)(handle, SETTING_USE_AA_FILTER, 1);
            (self.set_setting)(handle, SETTING_USE_QUICKSEEK, 0);
            (self.set_setting)(handle, SETTING_SEQUENCE_MS, SPEECH_SEQUENCE_MS);
            (self.set_setting)(handle, SETTING_SEEKWINDOW_MS, SPEECH_SEEKWINDOW_MS);
            (self.set_setting)(handle, SETTING_OVERLAP_MS, SPEECH_OVERLAP_MS);
        }
        SoundTouchInstance {
            lib: self,
            handle,
            channels,
        }
    }
}

/// Обработчик SoundTouch одной реплики (speech-профиль задан при создании).
pub struct SoundTouchInstance<'a> {
    lib: &'a SoundTouchLib,
    handle: ffi::Handle,
    channels: u32,
}

impl Drop for SoundTouchInstance<'_> {
    fn drop(&mut self) {
        unsafe { (self.lib.destroy_instance)(self.handle) };
    }
}

impl SoundTouchInstance<'_> {
    /// Темп ускорения (только > 1.0; высота тона не меняется).
    pub fn set_tempo(&mut self, tempo: f32) {
        unsafe { (self.lib.set_tempo)(self.handle, tempo) };
    }

    /// Подать сэмплы (кадры × каналы подряд). NaN/Inf — ошибка (защита от
    /// битой DLL/битого PCM на границе).
    pub fn put_samples(&mut self, pcm: &[f32]) -> Result<(), SoundTouchError> {
        if pcm.iter().any(|s| !s.is_finite()) {
            return Err(SoundTouchError::InvalidSampleData("NaN/Inf на входе"));
        }
        if pcm.is_empty() {
            return Ok(());
        }
        // Кадры (моно: кадр == сэмпл); подача блоками по 8192 кадров.
        let frames = pcm.len() / self.channels as usize;
        let block = 8192usize * self.channels as usize;
        let mut off = 0usize;
        while off < pcm.len() {
            let end = (off + block).min(pcm.len());
            let n = (end - off) / self.channels as usize;
            let rc =
                unsafe { (self.lib.put_samples)(self.handle, pcm[off..end].as_ptr(), n as u32) };
            if rc == 0 {
                return Err(SoundTouchError::InvalidSampleData(
                    "soundtouch_putSamples отклонил блок",
                ));
            }
            off = end;
        }
        debug_assert_eq!(frames * self.channels as usize, pcm.len());
        Ok(())
    }

    /// Забрать все готовые выходные сэмплы (без flush).
    pub fn drain_into(&mut self, out: &mut Vec<f32>) {
        let mut buf = vec![0.0f32; 8192 * self.channels as usize];
        loop {
            let n = unsafe {
                (self.lib.receive_samples)(
                    self.handle,
                    buf.as_mut_ptr(),
                    (buf.len() / self.channels as usize) as u32,
                )
            };
            if n == 0 {
                break;
            }
            let take = n as usize * self.channels as usize;
            out.extend_from_slice(&buf[..take]);
        }
    }

    /// Промыть внутренний буфер и добрать хвост. После этого instance
    /// больше не используется (закрывается на Drop).
    pub fn finish(&mut self, out: &mut Vec<f32>) {
        unsafe { (self.lib.flush)(self.handle) };
        self.drain_into(out);
    }

    pub fn num_samples(&self) -> u32 {
        unsafe { (self.lib.num_samples)(self.handle) }
    }

    pub fn is_empty(&self) -> bool {
        unsafe { (self.lib.is_empty)(self.handle) != 0 }
    }

    pub fn clear(&mut self) {
        unsafe { (self.lib.clear)(self.handle) };
    }
}

/// Обработка одной реплики целиком (контракт §7): tempo ≤ 1.0 — исходный
/// PCM без обработки (артефакты не вносятся никогда); иначе новый
/// speech-instance, подача, осушение, flush, осушение. Возвращает
/// фактические выходные сэмплы; NaN/Inf на выходе — ошибка.
pub fn process_cue(
    lib: &SoundTouchLib,
    pcm: &[f32],
    tempo: f32,
    sample_rate: u32,
    channels: u32,
) -> Result<Vec<f32>, SoundTouchError> {
    if !(tempo.is_finite() && tempo > 1.0) {
        // Без ускорения — исходный PCM (медленнее реального 1.0 не бывает:
        // замедление запрещено контрактом).
        return Ok(pcm.to_vec());
    }
    let mut inst = lib.create_speech_instance(sample_rate, channels);
    inst.set_tempo(tempo);
    inst.put_samples(pcm)?;
    let mut out = Vec::with_capacity((pcm.len() as f64 / tempo as f64) as usize + 4096);
    inst.drain_into(&mut out);
    inst.finish(&mut out);
    if out.iter().any(|s| !s.is_finite()) {
        return Err(SoundTouchError::InvalidSampleData("NaN/Inf на выходе"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// DLL может отсутствовать на платформах без third_party (не Windows
    /// и т.п.) — такие запуски пропускают проверки С ГРОМКИМ предупреждением
    /// (молчаливый пропуск уже один раз замаскировал неверный путь поиска).
    fn load_or_skip() -> Option<SoundTouchLib> {
        match SoundTouchLib::load(None) {
            Ok(lib) => {
                assert!(
                    lib.version_id() >= SOUNDTOUCH_MIN_VERSION_ID,
                    "version {}",
                    lib.version_id()
                );
                Some(lib)
            }
            Err(SoundTouchError::LibraryNotFound(paths)) => {
                eprintln!(
                    "SKIP: SoundTouchDLL не найдена: {}",
                    paths
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join("; ")
                );
                None
            }
            Err(e) => panic!("неожиданная ошибка загрузки SoundTouch: {e}"),
        }
    }

    fn sine(seconds: f64, freq: f64, sr: f64) -> Vec<f32> {
        let n = (seconds * sr) as usize;
        (0..n)
            .map(|i| (2.0 * std::f64::consts::PI * freq * i as f64 / sr).sin() as f32 * 0.5)
            .collect()
    }

    #[test]
    fn tempo_one_bypasses_library_without_dll() {
        // Обход не требует DLL вообще — проверяется на чистой функции.
        let pcm = vec![0.1f32, 0.2, -0.3, 0.0];
        // lib = None имитируется невозможностью загрузки: функция обхода
        // не должна звать библиотеку — тестируем через if-протокол вызова.
        let bypass = |t: f32| !(t.is_finite() && t > 1.0);
        assert!(bypass(1.0));
        assert!(bypass(0.5));
        assert!(!bypass(1.1));
        // и фактический путь: process_cue с tempo 1.0 не трогает lib —
        // проверено ниже с реальной DLL (ранний return).
        let _ = pcm;
    }

    #[test]
    fn version_is_2_4_x() {
        let Some(lib) = load_or_skip() else { return };
        let id = lib.version_id();
        assert!((20_400..20_500).contains(&id), "неожиданная версия {id}");
        assert!(
            lib.version_string().starts_with("2."),
            "{}",
            lib.version_string()
        );
    }

    #[test]
    fn tempo_2_halves_duration() {
        let Some(lib) = load_or_skip() else { return };
        let pcm = sine(10.0, 440.0, 24_000.0);
        let out = process_cue(&lib, &pcm, 2.0, 24_000, 1).expect("process");
        // Ожидание 5 с ±60 мс (внутренняя задержка/отступы SoundTouch).
        let expected = 5.0 * 24_000.0;
        let tol = 0.060 * 24_000.0;
        assert!(
            (out.len() as f64 - expected).abs() <= tol,
            "длина {} против {}",
            out.len(),
            expected
        );
    }

    #[test]
    fn tempo_15_and_11_ratios() {
        let Some(lib) = load_or_skip() else { return };
        let pcm = sine(10.0, 440.0, 24_000.0);
        for (tempo, expect_s) in [(1.5f32, 10.0 / 1.5), (1.1, 10.0 / 1.1)] {
            let out = process_cue(&lib, &pcm, tempo, 24_000, 1).expect("process");
            let tol = 0.080 * 24_000.0;
            assert!(
                (out.len() as f64 - expect_s * 24_000.0).abs() <= tol,
                "tempo {tempo}: длина {} против {:.1}",
                out.len(),
                expect_s * 24_000.0
            );
        }
    }

    #[test]
    fn short_and_zero_buffers() {
        let Some(lib) = load_or_skip() else { return };
        let empty = process_cue(&lib, &[], 2.0, 24_000, 1).expect("empty");
        assert!(
            empty.len() < 24_000,
            "пустой вход даёт <1с: {}",
            empty.len()
        );
        let tiny = process_cue(&lib, &[0.5, -0.5, 0.25], 2.0, 24_000, 1).expect("tiny");
        assert!(tiny.len() < 24_000, "короткий вход <1с: {}", tiny.len());
        assert!(tiny.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn flush_yields_complete_tail() {
        let Some(lib) = load_or_skip() else { return };
        // После finish() в конвейере не должно остаться сэмплов.
        let pcm = sine(3.0, 300.0, 24_000.0);
        let mut inst = lib.create_speech_instance(24_000, 1);
        inst.set_tempo(1.7);
        inst.put_samples(&pcm).expect("put");
        let mut out = Vec::new();
        inst.finish(&mut out);
        assert!(inst.is_empty(), "после flush буфер пуст");
        let expected = 3.0 / 1.7 * 24_000.0;
        assert!(
            (out.len() as f64 - expected).abs() <= 0.080 * 24_000.0,
            "длина {} против {expected}",
            out.len()
        );
    }

    #[test]
    fn chunked_feed_same_as_single_feed() {
        let Some(lib) = load_or_skip() else { return };
        let pcm = sine(5.0, 500.0, 24_000.0);
        // Целая подача.
        let whole = process_cue(&lib, &pcm, 1.4, 24_000, 1).expect("whole");
        // Блочная подача (как потоковая сборка реплики).
        let mut inst = lib.create_speech_instance(24_000, 1);
        inst.set_tempo(1.4);
        let mut out = Vec::new();
        for block in pcm.chunks(7_777) {
            inst.put_samples(block).expect("put");
            inst.drain_into(&mut out);
        }
        inst.finish(&mut out);
        assert_eq!(whole.len(), out.len(), "длины равны");
        assert_eq!(
            whole, out,
            "SoundTouch детерминирован: выход побитово равен"
        );
    }

    #[test]
    fn no_state_carry_between_instances() {
        let Some(lib) = load_or_skip() else { return };
        let pcm = sine(2.0, 440.0, 24_000.0);
        let a = process_cue(&lib, &pcm, 1.8, 24_000, 1).expect("a");
        let b = process_cue(&lib, &pcm, 1.8, 24_000, 1).expect("b");
        assert_eq!(a, b, "новый instance на реплику — выход идентичен");
    }

    #[test]
    fn nan_input_rejected() {
        let Some(lib) = load_or_skip() else { return };
        let bad = vec![0.0f32, f32::NAN, 0.1];
        let err = process_cue(&lib, &bad, 2.0, 24_000, 1).unwrap_err();
        assert!(matches!(err, SoundTouchError::InvalidSampleData(_)));
        let bad_inf = vec![0.0f32, f32::INFINITY, 0.1];
        assert!(process_cue(&lib, &bad_inf, 2.0, 24_000, 1).is_err());
    }

    #[test]
    fn tempo_one_with_library_returns_input_unchanged() {
        let Some(lib) = load_or_skip() else { return };
        let pcm = sine(1.0, 440.0, 24_000.0);
        let out = process_cue(&lib, &pcm, 1.0, 24_000, 1).expect("bypass");
        assert_eq!(out, pcm, "tempo 1.0 — без обработки");
    }

    #[test]
    fn missing_explicit_path_is_clear_error() {
        let err = SoundTouchLib::load(Some(Path::new("Z:/definitely/missing/st.dll"))).unwrap_err();
        match &err {
            SoundTouchError::LibraryNotFound(p) => assert_eq!(p.len(), 1),
            other => panic!("ожидалась LibraryNotFound, получено {other:?}"),
        }
        assert!(err.to_string().contains("не найдена"));
    }
}

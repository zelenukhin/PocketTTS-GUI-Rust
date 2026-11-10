//! Диагностика для smoke/bench: память процесса (Private Bytes, Working Set,
//! Peak) и VRAM через nvidia-smi. Никаких сторонних зависимостей уровня системы.

#[derive(Debug, Clone, Default)]
pub struct ProcessMemory {
    pub working_set_mb: f64,
    pub peak_working_set_mb: f64,
    pub private_mb: f64,
}

#[cfg(windows)]
pub fn process_memory() -> std::io::Result<ProcessMemory> {
    use windows_sys::Win32::System::ProcessStatus::GetProcessMemoryInfo;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    #[allow(non_snake_case)]
    #[repr(C)]
    struct PROCESS_MEMORY_COUNTERS_EX {
        cb: u32,
        PageFaultCount: u32,
        PeakWorkingSetSize: usize,
        WorkingSetSize: usize,
        QuotaPeakPagedPoolUsage: usize,
        QuotaPagedPoolUsage: usize,
        QuotaPeakNonPagedPoolUsage: usize,
        QuotaNonPagedPoolUsage: usize,
        PagefileUsage: usize,
        PeakPagefileUsage: usize,
        PrivateUsage: usize,
    }

    let mut pmc = PROCESS_MEMORY_COUNTERS_EX {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        PageFaultCount: 0,
        PeakWorkingSetSize: 0,
        WorkingSetSize: 0,
        QuotaPeakPagedPoolUsage: 0,
        QuotaPagedPoolUsage: 0,
        QuotaPeakNonPagedPoolUsage: 0,
        QuotaNonPagedPoolUsage: 0,
        PagefileUsage: 0,
        PeakPagefileUsage: 0,
        PrivateUsage: 0,
    };
    let handle = unsafe { GetCurrentProcess() };
    let ok = unsafe { GetProcessMemoryInfo(handle, &mut pmc as *mut _ as *mut _, pmc.cb) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    const MB: f64 = 1024.0 * 1024.0;
    Ok(ProcessMemory {
        working_set_mb: pmc.WorkingSetSize as f64 / MB,
        peak_working_set_mb: pmc.PeakWorkingSetSize as f64 / MB,
        private_mb: pmc.PrivateUsage as f64 / MB,
    })
}

#[cfg(not(windows))]
pub fn process_memory() -> std::io::Result<ProcessMemory> {
    Ok(ProcessMemory::default())
}

/// Использованная VRAM (МБ) нашего процесса по nvidia-smi; 0.0 — если процесса
/// нет в списке compute-apps или утилита недоступна (не ошибка).
pub fn vram_used_mb(pid: u32) -> f64 {
    let out = std::process::Command::new("nvidia-smi")
        .args([
            "--query-compute-apps=pid,used_memory",
            "--format=csv,noheader,nounits",
        ])
        .output();
    let Ok(out) = out else { return 0.0 };
    if !out.status.success() {
        return 0.0;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        let mut it = line.split(',');
        let (Some(p), Some(m)) = (it.next(), it.next()) else {
            continue;
        };
        if p.trim().parse::<u32>().ok() == Some(pid) {
            return m.trim().parse::<f64>().unwrap_or(0.0);
        }
    }
    0.0
}

/// Суммарная использованная VRAM GPU 0 (МБ); None — nvidia-smi недоступен.
pub fn gpu_total_used_mb() -> Option<f64> {
    let out = std::process::Command::new("nvidia-smi")
        .args([
            "--query-gpu=memory.used",
            "--format=csv,noheader,nounits",
            "-i",
            "0",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines().next()?.trim().parse::<f64>().ok()
}

/// Свободная VRAM указанного GPU (МБ); None — nvidia-smi недоступен.
/// Используется ТОЛЬКО как префлайт перед созданием N-го CUDA-воркера
/// (G6.1): недостаток памяти — не абсолютная защита от OOM, а способ
/// заранее выбрать экономную конфигурацию с понятной записью в журнале.
pub fn gpu_free_mb(device_id: i32) -> Option<f64> {
    let out = std::process::Command::new("nvidia-smi")
        .args([
            "--query-gpu=memory.free",
            "--format=csv,noheader,nounits",
            "-i",
            &device_id.to_string(),
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines().next()?.trim().parse::<f64>().ok()
}

pub fn pid() -> u32 {
    std::process::id()
}

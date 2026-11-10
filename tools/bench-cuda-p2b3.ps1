# CUDA-P2B.3: launcher матрицы гибридного параллелизма на пакете файлов.
#
# Все режимы (кроме ref) — diag_p2b3.exe hybrid: N процессов × W постоянных
# CUDA-воркеров, общая claim-очередь файлов, гейт готовности между процессами:
#   a1  1×1   a2 1×2   a3 1×3   a4 1×4     (один процесс, N воркеров)
#   b2  2×1   b4 4×1                    (N процессов, один воркер)
#   h22 2×2                              (гибрид: 2 процесса × 2 воркера)
# ref: эталон одиночного запуска — по одному synthesize.exe на файл,
#   последовательно (модель загружается на каждый файл).
#
# Перед каждым прогоном — VRAM-префлайт (nvidia-smi): свободная память
# сравнивается с оценкой под все Pocket (≈2.3 ГиБ/Pocket + 0.8 ГиБ базовых и
# промежуточных тензоров); при нехватке прогон пропускается и фиксируется в
# run.json (skipped_vram), процессы не запускаются.
#
# stdout/stderr дочерних процессов пишутся побайтово в файлы через
# CopyToAsync. Код завершения — из System.Diagnostics.Process.
# Мониторы GPU/PCIe/CPU — те же, что в P2B.1/P2B.2.
# Безопасность: завершаются только собственные дочерние процессы; никаких
# taskkill /IM, рекурсивных удалений и перезаписи существующих файлов.
#
# usage:
#   powershell -NoProfile -ExecutionPolicy Bypass -File tools\bench-cuda-p2b3.ps1 -Mode h22 -Rounds r1 -PartsDir out\cuda_p2b3\main12_parts_<stamp>
#   ... -Mode a4 -Threads 2 -OutRoot out\cuda_p2b3\main12_t2 ...
#   ... -Mode ref -Reps 1 -SkipWarmup
param(
    [Parameter(Mandatory)][ValidateSet('a1', 'a2', 'a3', 'a4', 'b2', 'b4', 'h22', 'ref')][string]$Mode,
    [int]$Reps = 3,
    [switch]$SkipWarmup,
    [switch]$NoMonitor,
    [int]$TimeoutSec = 900,
    [int]$Threads = 4,
    [string]$PartsDir = '',
    [string]$OutRoot = '',
    # Явный список раундов (например 'warmup' или 'r1'); пусто — warmup + Reps
    [string[]]$Rounds = @(),
    # Голосовой WAV (по умолчанию — эталон P2B.1/P2B.2)
    [string]$Voice = ''
)
$ErrorActionPreference = 'Stop'

$repo = Split-Path -Parent $PSScriptRoot
$exe = Join-Path $repo 'target\release\diag_p2b3.exe'
$synth = Join-Path $repo 'target\release\synthesize.exe'
$modelDir = 'C:/Users/alexa/Downloads/pocket_tts_android/build/onnx/ru-v2-slow'
$voice = 'C:/PocketTTS-GUI/examples/Василий Леванов F5-TTS3.wav'
if (-not [string]::IsNullOrWhiteSpace($Voice)) { $voice = $Voice }
$charRules = 'C:/PocketTTS-GUI/PocketTTS-GUI/custom_models/Shtorm_PocketTTS_RU_v2_slow/runtime.json'
$psExe = (Get-Command powershell.exe).Source
$gpuMon = Join-Path $PSScriptRoot 'bench-cuda-gpu-monitor.ps1'
$cpuMon = Join-Path $PSScriptRoot 'bench-cuda-cpu-sampler.ps1'
$utf8NoBom = New-Object System.Text.UTF8Encoding($false)

if ([string]::IsNullOrWhiteSpace($OutRoot)) { $OutRoot = Join-Path $repo 'out\cuda_p2b3' }
if ([string]::IsNullOrWhiteSpace($PartsDir)) { throw "укажите -PartsDir (каталог part_*.txt)" }
if (-not (Test-Path -LiteralPath $PartsDir)) { throw "нет каталога частей: $PartsDir" }
$files = @(Get-ChildItem -LiteralPath $PartsDir -Filter 'part_*.txt' | Sort-Object Name | ForEach-Object { $_.FullName })
if ($files.Count -lt 2) { throw "в $PartsDir меньше двух part_*.txt" }
foreach ($p in @($exe, $synth, $charRules) + $files) {
    if (-not (Test-Path -LiteralPath $p)) { throw "нет файла: $p" }
}
if (-not (Test-Path -LiteralPath $modelDir)) { throw "нет каталога модели: $modelDir" }
if (-not (Test-Path -LiteralPath $voice)) { throw "нет голоса: $voice" }

switch ($Mode) {
    'a1'  { $processCount = 1; $workers = 1 }
    'a2'  { $processCount = 1; $workers = 2 }
    'a3'  { $processCount = 1; $workers = 3 }
    'a4'  { $processCount = 1; $workers = 4 }
    'b2'  { $processCount = 2; $workers = 1 }
    'b4'  { $processCount = 4; $workers = 1 }
    'h22' { $processCount = 2; $workers = 2 }
    'ref' { $processCount = 1; $workers = 1 }
}
$pockets = $processCount * $workers
# Оценка VRAM: ~2.13 ГиБ/Pocket (замер P2B.2 A3) + запас на baseline GPU
# (~0.55 ГиБ) и промежуточные тензоры; округлено вверх до 2.35 ГиБ/Pocket
# + 0.9 ГиБ. RTX 3060 12 ГиБ: 4 Pocket ≈ 10.3 ГиБ — проходит только при
# свободной шине без посторонних GPU-процессов.
$requiredVramMb = [int]($pockets * 2355 + 900)

$invStamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$labelDir = Join-Path $OutRoot ($Mode + '_t' + $Threads)
if (-not (Test-Path -LiteralPath $labelDir)) { New-Item -ItemType Directory -Path $labelDir | Out-Null }
$invDir = Join-Path $labelDir $invStamp
if (Test-Path -LiteralPath $invDir) { throw "каталог вызова уже существует (create-new): $invDir" }
New-Item -ItemType Directory -Path $invDir | Out-Null

# Окружение дочерних процессов — как в P2B.1/P2B.2 (auto = CUDA I/O Binding).
$env:PATH = (Join-Path $repo 'deps\portable') + ';' + $env:PATH
$env:ORT_DYLIB_PATH = (Join-Path $repo 'deps\portable\onnxruntime.dll')
$env:SHTORM_PHASE_TIMING = '1'
$env:RUST_LOG = 'info'
Remove-Item Env:SHTORM_CUDA_IO_BINDING -ErrorAction SilentlyContinue
Remove-Item Env:SHTORM_TEST_CANCEL_AFTER_MS -ErrorAction SilentlyContinue

function Start-Monitors([string]$roundDir) {
    $stop = Join-Path $roundDir 'monitor.stop'
    $gpuArgs = '-NoProfile -ExecutionPolicy Bypass -File "{0}" -OutGpuCsv "{1}" -OutPcieCsv "{2}" -StopFile "{3}"' -f `
        $gpuMon, (Join-Path $roundDir 'gpu.csv'), (Join-Path $roundDir 'pcie.csv'), $stop
    $g = Start-Process -FilePath $psExe -ArgumentList $gpuArgs -NoNewWindow -PassThru
    $cpuStop = Join-Path $roundDir 'cpu.stop'
    $cpuArgs = '-NoProfile -ExecutionPolicy Bypass -File "{0}" -OutCsv "{1}" -StopFile "{2}"' -f `
        $cpuMon, (Join-Path $roundDir 'cpu.csv'), $cpuStop
    $c = Start-Process -FilePath $psExe -ArgumentList $cpuArgs -NoNewWindow -PassThru
    return [pscustomobject]@{ gpu = $g; cpu = $c; stop = $stop; cpuStop = $cpuStop }
}

function Stop-Monitors($m) {
    if ($null -eq $m) { return }
    try { if (-not (Test-Path -LiteralPath $m.stop)) { [System.IO.File]::WriteAllText($m.stop, '') } } catch { }
    try { if (-not (Test-Path -LiteralPath $m.cpuStop)) { [System.IO.File]::WriteAllText($m.cpuStop, '') } } catch { }
    foreach ($p in @($m.gpu, $m.cpu)) {
        if ($null -ne $p -and -not $p.HasExited) {
            if (-not $p.WaitForExit(10000)) {
                try { $p.Kill() } catch { }  # только собственный дочерний процесс
            }
        }
    }
}

# Запуск дочернего процесса с побайтовой записью stdout/stderr в файлы.
function Start-Child([string]$file, [string]$arguments, [string]$outLog, [string]$errLog) {
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $file
    $psi.Arguments = $arguments
    $psi.WorkingDirectory = $repo
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.CreateNoWindow = $true
    $p = [System.Diagnostics.Process]::Start($psi)
    $outFs = [System.IO.File]::Open($outLog, 'CreateNew', 'Write', 'Read')
    $errFs = [System.IO.File]::Open($errLog, 'CreateNew', 'Write', 'Read')
    $ot = $p.StandardOutput.BaseStream.CopyToAsync($outFs)
    $et = $p.StandardError.BaseStream.CopyToAsync($errFs)
    return [pscustomobject]@{
        p = $p; outFs = $outFs; errFs = $errFs; ot = $ot; et = $et
        outLog = $outLog; errLog = $errLog; done = $false
        exitMs = $null; exitUtcMs = $null; timedOut = $false; rc = $null
    }
}

# Дождаться завершения (или таймаута), закрыть файлы, снять код возврата.
function Complete-Child($c, [int]$timeoutMs) {
    $ok = $c.p.WaitForExit($timeoutMs)
    if (-not $ok) {
        $c.timedOut = $true
        try { $c.p.Kill() } catch { }
        $c.p.WaitForExit(5000) | Out-Null
    }
    $c.exitMs = $script:sw.ElapsedMilliseconds
    $c.exitUtcMs = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
    try { [void]$c.ot.Wait(15000) } catch { }
    try { [void]$c.et.Wait(15000) } catch { }
    try { $c.outFs.Dispose() } catch { }
    try { $c.errFs.Dispose() } catch { }
    try { $c.rc = $c.p.ExitCode } catch { $c.rc = $null }
    $c.done = $true
    return $c
}

function Get-WavInfo([string]$path) {
    if (-not (Test-Path -LiteralPath $path)) {
        return [ordered]@{ path = $path; exists = $false }
    }
    $bytes = (Get-Item -LiteralPath $path).Length
    $fs = [System.IO.File]::OpenRead($path)
    try {
        $hdr = New-Object byte[] 12
        $n = $fs.Read($hdr, 0, 12)
    } finally { $fs.Dispose() }
    $riff = ($n -ge 12 -and $hdr[0] -eq 0x52 -and $hdr[1] -eq 0x49 -and $hdr[2] -eq 0x46 -and $hdr[3] -eq 0x46)
    $wave = ($n -ge 12 -and $hdr[8] -eq 0x57 -and $hdr[9] -eq 0x41 -and $hdr[10] -eq 0x56 -and $hdr[11] -eq 0x45)
    $sha = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLower()
    return [ordered]@{
        path = $path
        name = [System.IO.Path]::GetFileName($path)
        exists = $true
        bytes = $bytes
        riff_ok = [bool]$riff
        wave_ok = [bool]$wave
        # PCM16 mono 24 кГц: (байты - 44) / 2 / 24000
        duration_s = [math]::Round((($bytes - 44) / 48000.0), 3)
        sha256 = $sha
    }
}

# Число строк `load <graph>: N ms` = число загруженных ONNX-сессий в процессе
# (5 на каждый Pocket). Скрытых пересозданий сессий быть не должно.
function Get-LoadLines([string]$errLog) {
    if (-not (Test-Path -LiteralPath $errLog)) { return 0 }
    $m = Select-String -LiteralPath $errLog -Pattern 'load (mimi_encoder|text_conditioner|flow_lm_main|flow_lm_flow|mimi_decoder):' -AllMatches
    $count = 0
    foreach ($x in $m) { $count += $x.Matches.Count }
    return $count
}

# VRAM-префлайт: свободная память и фон ДО старта процессов.
function Get-VramPreflight {
    try {
        $q = & nvidia-smi --query-gpu=memory.free,memory.used,utilization.gpu,temperature.gpu --format=csv,noheader,nounits
        $p = ($q -split ',') | ForEach-Object { $_.Trim() }
        return [ordered]@{
            free_mb = [int]$p[0]; used_mb = [int]$p[1]
            util_pct = [int]$p[2]; temp_c = [int]$p[3]
            required_mb = $requiredVramMb; pockets = $pockets
            ok = ([int]$p[0] -ge $requiredVramMb)
        }
    } catch {
        return [ordered]@{ error = "nvidia-smi: $_"; required_mb = $requiredVramMb; pockets = $pockets; ok = $false }
    }
}

function Invoke-Round([string]$name) {
    $roundDir = Join-Path $invDir $name
    if (Test-Path -LiteralPath $roundDir) { throw "каталог прогона уже существует: $roundDir" }
    New-Item -ItemType Directory -Path $roundDir | Out-Null
    $outDir = Join-Path $roundDir 'out'
    $queueDir = Join-Path $roundDir 'queue'
    $evDir = Join-Path $roundDir 'events'
    New-Item -ItemType Directory -Path $evDir | Out-Null

    $preflight = Get-VramPreflight
    if (-not $preflight.ok -and $Mode -ne 'ref') {
        # Недостаточно VRAM: новые задания не запускаем, фиксируем пропуск.
        $doc = [ordered]@{
            mode = $Mode; round = $name; processes = $processCount; workers = $workers
            pockets = $pockets; threads = $Threads; skipped_vram = $true
            vram_preflight = $preflight; parts_dir = $PartsDir; files = @($files)
        }
        [System.IO.File]::WriteAllText((Join-Path $roundDir 'run.json'), ($doc | ConvertTo-Json -Depth 8), $utf8NoBom)
        Write-Output ('=== {0}/{1}: ПРОПУЩЕН (VRAM свободно {2} МБ < {3} МБ на {4} Pocket)' -f $Mode, $name, $preflight.free_mb, $preflight.required_mb, $pockets)
        return
    }

    $mon = $null
    if (-not $NoMonitor) {
        $mon = Start-Monitors $roundDir
        Start-Sleep -Milliseconds 1500  # фон до старта: базовая линия GPU/CPU
    }

    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $script:sw = $sw
    $absStart = [DateTimeOffset]::UtcNow
    $procs = New-Object System.Collections.ArrayList
    $filesArg = ($files | ForEach-Object { '"{0}"' -f $_ }) -join ' '
    $common = '--model-dir "{0}" --voice "{1}" --files {2} --out-dir "{3}" --device cuda --threads {4} --seed 42 --char-rules "{5}" --temperature 0.3 --decode-steps 1 --eos-threshold -4 --eos-tail-short 5 --eos-tail-long 3 --brightness-db 8 --pause-scale 1.0 --paragraph-pause-s 0.9 --max-tokens 50' -f `
        $modelDir, $voice, $filesArg, $outDir, $Threads, $charRules

    if ($Mode -eq 'ref') {
        # Эталон: отдельный запуск CLI на каждый файл, последовательно
        for ($i = 0; $i -lt $files.Count; $i++) {
            $wav = Join-Path $roundDir ('ref_part_{0:00}.wav' -f ($i + 1))
            $outLog = Join-Path $roundDir ('ref_p{0}.stdout.log' -f $i)
            $errLog = Join-Path $roundDir ('ref_p{0}.stderr.log' -f $i)
            $argLine = '--model-dir "{0}" --voice "{1}" --text-file "{2}" --out "{3}" --format wav --device cuda --workers 1 --threads {4} --seed 42 --char-rules "{5}" --temperature 0.3 --decode-steps 1 --eos-threshold -4 --eos-tail-short 5 --eos-tail-long 3 --brightness-db 8 --pause-scale 1.0 --paragraph-pause-s 0.9 --max-tokens 50' -f `
                $modelDir, $voice, $files[$i], $wav, $Threads, $charRules
            $spawnMs = $sw.ElapsedMilliseconds
            $spawnUtc = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
            $c = Start-Child $synth $argLine $outLog $errLog
            $c | Add-Member -NotePropertyName index -NotePropertyValue $i
            $c | Add-Member -NotePropertyName spawnMs -NotePropertyValue $spawnMs
            $c | Add-Member -NotePropertyName spawnUtc -NotePropertyValue $spawnUtc
            $c | Add-Member -NotePropertyName tag -NotePropertyValue ('ref_part_{0:00}' -f ($i + 1))
            $c | Add-Member -NotePropertyName command -NotePropertyValue ('synthesize.exe ' + $argLine)
            Complete-Child $c ($TimeoutSec * 1000) | Out-Null
            [void]$procs.Add($c)
        }
    }
    else {
        # a*/b*/h22: N процессов diag_p2b3 hybrid, общая очередь, гейт готовности
        $running = New-Object System.Collections.ArrayList
        for ($i = 0; $i -lt $processCount; $i++) {
            $argLine = 'hybrid ' + $common + ' --workers {0} --expect-processes {1} --queue-dir "{2}" --events "{3}" --tag "{4}"' -f `
                $workers, $processCount, $queueDir, (Join-Path $evDir ('events_p{0}.jsonl' -f $i)), ('{0}_p{1}' -f $Mode, $i)
            $outLog = Join-Path $roundDir ('p{0}.stdout.log' -f $i)
            $errLog = Join-Path $roundDir ('p{0}.stderr.log' -f $i)
            $spawnMs = $sw.ElapsedMilliseconds
            $spawnUtc = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
            $c = Start-Child $exe $argLine $outLog $errLog
            $c | Add-Member -NotePropertyName index -NotePropertyValue $i
            $c | Add-Member -NotePropertyName spawnMs -NotePropertyValue $spawnMs
            $c | Add-Member -NotePropertyName spawnUtc -NotePropertyValue $spawnUtc
            $c | Add-Member -NotePropertyName tag -NotePropertyValue ('{0}_p{1}' -f $Mode, $i)
            $c | Add-Member -NotePropertyName command -NotePropertyValue ('diag_p2b3 ' + $argLine)
            [void]$running.Add($c)
        }
        $deadline = [TimeSpan]::FromSeconds($TimeoutSec)
        while ($true) {
            $alive = 0
            foreach ($r in $running) {
                if (-not $r.done) {
                    if ($r.p.WaitForExit(50)) {
                        $r.exitMs = $sw.ElapsedMilliseconds
                        $r.exitUtcMs = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
                        $r.done = $true
                    } else {
                        $alive++
                    }
                }
            }
            if ($alive -eq 0) { break }
            if ($sw.Elapsed -gt $deadline) {
                foreach ($r in $running) {
                    if (-not $r.done) {
                        $r.timedOut = $true
                        try { $r.p.Kill() } catch { }
                        $r.p.WaitForExit(5000) | Out-Null
                        $r.exitUtcMs = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
                        $r.done = $true
                    }
                }
                break
            }
        }
        foreach ($r in $running) {
            try { [void]$r.ot.Wait(15000) } catch { }
            try { [void]$r.et.Wait(15000) } catch { }
            try { $r.outFs.Dispose() } catch { }
            try { $r.errFs.Dispose() } catch { }
            try { $r.rc = $r.p.ExitCode } catch { $r.rc = $null }
            [void]$procs.Add($r)
        }
    }

    $endMs = $sw.ElapsedMilliseconds
    $absEnd = [DateTimeOffset]::UtcNow
    if (-not $NoMonitor) { Stop-Monitors $mon }

    $wavs = @()
    if ($Mode -eq 'ref') {
        $list = Get-ChildItem -LiteralPath $roundDir -Filter '*.wav' | Sort-Object Name
    } else {
        New-Item -ItemType Directory -Path $outDir -Force | Out-Null
        $list = Get-ChildItem -LiteralPath $outDir -Filter '*.wav' | Sort-Object Name
    }
    foreach ($w in $list) { $wavs += Get-WavInfo $w.FullName }

    $procsOut = @()
    foreach ($c in $procs) {
        $procsOut += [ordered]@{
            index = $c.index; pid = $c.p.Id; tag = $c.tag
            spawn_ms = $c.spawnMs; exit_ms = $c.exitMs
            spawn_utc_ms = $c.spawnUtc; exit_utc_ms = $c.exitUtcMs
            rc = $c.rc; timed_out = [bool]$c.timedOut
            stdout = $c.outLog; stderr = $c.errLog
            onnx_session_loads = (Get-LoadLines $c.errLog)
            command = $c.command
        }
    }

    $doc = [ordered]@{
        mode = $Mode; round = $name; processes = $processCount; workers = $workers
        pockets = $pockets; threads = $Threads; skipped_vram = $false
        vram_preflight = $preflight
        parts_dir = $PartsDir; files = @($files)
        start_utc = $absStart.ToString('o'); end_utc = $absEnd.ToString('o')
        start_epoch_ms = $absStart.ToUnixTimeMilliseconds(); end_epoch_ms = $absEnd.ToUnixTimeMilliseconds()
        wall_ms = $endMs
        procs = $procsOut
        outputs = @($wavs)
        gpu_csv = (Join-Path $roundDir 'gpu.csv'); pcie_csv = (Join-Path $roundDir 'pcie.csv')
        cpu_csv = (Join-Path $roundDir 'cpu.csv')
    }
    $json = $doc | ConvertTo-Json -Depth 12
    [System.IO.File]::WriteAllText((Join-Path $roundDir 'run.json'), $json, $utf8NoBom)

    $brief = @()
    foreach ($p in $procsOut) { $brief += ('p{0}=rc{1}/{2}ms/loads{3}' -f $p.index, $p.rc, $p.exit_ms, $p.onnx_session_loads) }
    Write-Output ('=== {0}/{1}: {2}; wall {3} ms; wavs {4}; vram free {5}/{6} MB; dir {7}' -f $Mode, $name, ($brief -join ' '), $endMs, $wavs.Count, $preflight.free_mb, $preflight.required_mb, $roundDir)
}

# ВАЖНО: имена переменных PowerShell регистронезависимы, поэтому локальный
# список раундов не может называться $rounds — это тот же $Rounds-параметр.
$roundList = @()
if ($Rounds.Count -gt 0) {
    $roundList = $Rounds
} else {
    if (-not $SkipWarmup) { $roundList += 'warmup' }
    for ($r = 1; $r -le $Reps; $r++) { $roundList += ('r{0}' -f $r) }
}
foreach ($name in $roundList) { Invoke-Round $name }
Write-Output ('готово: {0} (parts: {1}; файлов: {2})' -f $invDir, $PartsDir, $files.Count)

# CUDA-P2B.1: launcher независимых процессов Rust (и групп воркеров).
#
# Запускает N процессов target\release\synthesize.exe, каждый со своим
# CUDA-воркером по умолчанию, своим WAV и своим stdout/stderr. Стартует
# процессы одной группы одновременно, чтобы окна синтеза перекрывались.
# Барьера после загрузки моделей в CLI нет (исходники на этом стенде не
# менялись), поэтому launcher измеряет появление строк `load <graph>: N ms`
# и `[k/N] готов` с точностью опроса ~25 мс: время загрузки и окно синтеза
# каждого процесса восстанавливаются из событий, а не из общего wall time.
#
# Безопасность: никаких taskkill /IM и рекурсивных удалений; завершаются
# только собственные дочерние процессы (Process.Kill по объекту).
#
# usage:
#   powershell -File tools\bench-cuda-multiprocess.ps1 -Label a1 -Processes 1 -Workers 1 -Reps 3
#   ... -Label a3 -Processes 3 -Workers 1 -Reps 3
#   ... -Label c_seq -Processes 3 -Workers 1 -Sequential -SkipWarmup -Reps 2
param(
    [Parameter(Mandatory)][string]$Label,
    [int]$Processes = 1,
    [int]$Workers = 1,
    [int]$Threads = 4,
    [int]$Reps = 3,
    [switch]$Sequential,
    [switch]$SkipWarmup,
    [switch]$NoMonitor,
    [int]$TimeoutSec = 900
)
$ErrorActionPreference = 'Stop'
$repo = 'C:\PocketTTS-GUI-Rust\ShtormTTS-Windows'
$exe = Join-Path $repo 'target\release\synthesize.exe'
$modelDir = 'C:/Users/alexa/Downloads/pocket_tts_android/build/onnx/ru-v2-slow'
$voice = 'C:/PocketTTS-GUI/examples/Василий Леванов F5-TTS3.wav'
$textFile = Join-Path $repo 'out\cuda_p1\test_text.txt'
$charRules = 'C:/PocketTTS-GUI/PocketTTS-GUI/custom_models/Shtorm_PocketTTS_RU_v2_slow/runtime.json'
$psExe = (Get-Command powershell.exe).Source
$gpuMon = Join-Path $repo 'tools\bench-cuda-gpu-monitor.ps1'
$cpuMon = Join-Path $repo 'tools\bench-cuda-cpu-sampler.ps1'
$utf8NoBom = New-Object System.Text.UTF8Encoding($false)

foreach ($p in @($exe, $textFile, $charRules)) {
    if (-not (Test-Path -LiteralPath $p)) { throw "нет файла: $p" }
}
if (-not (Test-Path -LiteralPath $modelDir)) { throw "нет каталога модели: $modelDir" }
if (-not (Test-Path -LiteralPath $voice)) { throw "нет голоса: $voice" }
if ($Processes -lt 1 -or $Processes -gt 3) { throw "Processes 1..3 (VRAM-политика)" }
if ($Workers -lt 1 -or $Workers -gt 3) { throw "Workers 1..3" }

$root = Join-Path $repo 'out\cuda_p2b1'
if (-not (Test-Path -LiteralPath $root)) { New-Item -ItemType Directory -Path $root | Out-Null }
$labelDir = Join-Path $root $Label
if (Test-Path -LiteralPath $labelDir) { throw "каталог уже существует (create-new): $labelDir" }
New-Item -ItemType Directory -Path $labelDir | Out-Null

# Окружение дочерних процессов (как в P2A.1: auto = binding по умолчанию).
$env:PATH = (Join-Path $repo 'deps\portable') + ';' + $env:PATH
$env:ORT_DYLIB_PATH = (Join-Path $repo 'deps\portable\onnxruntime.dll')
$env:SHTORM_PHASE_TIMING = '1'
Remove-Item Env:SHTORM_CUDA_IO_BINDING -ErrorAction SilentlyContinue
Remove-Item Env:SHTORM_TEST_CANCEL_AFTER_MS -ErrorAction SilentlyContinue

function Convert-EventLine([string]$line, [string]$src) {
    if ($line -match 'shtorm_engine::pocket\] load (\w+): (\d+) ms') {
        return [pscustomobject]@{ src = $src; kind = 'load'; graph = $Matches[1]; ms = [int]$Matches[2]; text = $line }
    }
    if ($line -match 'io binding active') {
        return [pscustomobject]@{ src = $src; kind = 'io_binding'; text = $line }
    }
    if ($line -match '^\s*\[(\d+)/(\d+)\] готов') {
        return [pscustomobject]@{ src = $src; kind = 'chunk_done'; index = [int]$Matches[1]; total = [int]$Matches[2]; text = $line }
    }
    if ($line -match '^файл: ') {
        return [pscustomobject]@{ src = $src; kind = 'final'; text = $line }
    }
    if ($line -match '^пул: воркеров') {
        return [pscustomobject]@{ src = $src; kind = 'pool'; text = $line }
    }
    if ($line -match '^phase \| chunk ') {
        return [pscustomobject]@{ src = $src; kind = 'phase_detail'; text = $line }
    }
    if ($line -match '^(финализация кодирования|публикация результата)') {
        return [pscustomobject]@{ src = $src; kind = 'phase'; text = $line }
    }
    if ($line -match '^(ОШИБКА|ОТМЕНЕНО|ВНИМАНИЕ|FATAL)') {
        return [pscustomobject]@{ src = $src; kind = 'error'; text = $line }
    }
    return $null
}

# Дренирует все уже готовые строки stdout/stderr без блокировки.
function Pump-One($it, [long]$tms) {
    for ($spin = 0; $spin -lt 1000; $spin++) {
        $tasks = New-Object System.Collections.ArrayList
        if ($null -ne $it.ot) { [void]$tasks.Add($it.ot) }
        if ($null -ne $it.et) { [void]$tasks.Add($it.et) }
        if ($tasks.Count -eq 0) { return }
        $idx = [System.Threading.Tasks.Task]::WaitAny([System.Threading.Tasks.Task[]]$tasks.ToArray(), 0)
        if ($idx -lt 0) { return }
        if ($null -ne $it.ot -and $it.ot.IsCompleted) {
            $line = $null
            try { $line = $it.ot.Result } catch { $line = $null }
            if ($null -eq $line) {
                $it.ot = $null
            } else {
                $it.outW.WriteLine($line)
                $ev = Convert-EventLine $line 'out'
                if ($null -ne $ev) { $ev | Add-Member -NotePropertyName t_ms -NotePropertyValue $tms; [void]$it.events.Add($ev) }
                $it.ot = $it.p.StandardOutput.ReadLineAsync()
            }
        }
        if ($null -ne $it.et -and $it.et.IsCompleted) {
            $line = $null
            try { $line = $it.et.Result } catch { $line = $null }
            if ($null -eq $line) {
                $it.et = $null
            } else {
                $it.errW.WriteLine($line)
                $ev = Convert-EventLine $line 'err'
                if ($null -ne $ev) { $ev | Add-Member -NotePropertyName t_ms -NotePropertyValue $tms; [void]$it.events.Add($ev) }
                $it.et = $it.p.StandardError.ReadLineAsync()
            }
        }
    }
}

function Drain-One($it, [long]$tms) {
    for ($k = 0; $k -lt 100; $k++) {
        Pump-One $it $tms
        if ($null -eq $it.ot -and $null -eq $it.et) { return }
        Start-Sleep -Milliseconds 20
    }
}

function Start-One([string]$roundDir, [int]$i) {
    $pdir = Join-Path $roundDir ("p{0}" -f $i)
    if (Test-Path -LiteralPath $pdir) { throw "каталог процесса уже существует: $pdir" }
    New-Item -ItemType Directory -Path $pdir | Out-Null
    $wav = Join-Path $pdir 'out.wav'
    $outLog = Join-Path $pdir 'stdout.log'
    $errLog = Join-Path $pdir 'stderr.log'
    $argLine = '--model-dir "{0}" --voice "{1}" --text-file "{2}" --out "{3}" ' -f $modelDir, $voice, $textFile, $wav
    $argLine += '--format wav --device cuda --workers {0} --threads {1} --seed 42 ' -f $Workers, $Threads
    $argLine += '--char-rules "{0}" --chunk-log' -f $charRules

    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $exe
    $psi.Arguments = $argLine
    $psi.WorkingDirectory = $repo
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.CreateNoWindow = $true
    $psi.StandardOutputEncoding = $utf8NoBom
    $psi.StandardErrorEncoding = $utf8NoBom
    $p = [System.Diagnostics.Process]::Start($psi)

    $outW = New-Object System.IO.StreamWriter([System.IO.File]::Open($outLog, 'CreateNew'), $utf8NoBom)
    $outW.AutoFlush = $true
    $errW = New-Object System.IO.StreamWriter([System.IO.File]::Open($errLog, 'CreateNew'), $utf8NoBom)
    $errW.AutoFlush = $true
    $ot = $p.StandardOutput.ReadLineAsync()
    $et = $p.StandardError.ReadLineAsync()

    return [pscustomobject]@{
        i = $i; pid = $p.Id; p = $p; wav = $wav; outLog = $outLog; errLog = $errLog
        outW = $outW; errW = $errW; ot = $ot; et = $et; closed = $false
        startMs = 0; exitMs = $null; rc = $null; timedOut = $false; exited = $false
        events = New-Object System.Collections.ArrayList
    }
}

function Stop-Tree([int]$procId) {
    # только собственное дерево (PID создан этим launcher'ом)
    try { & taskkill.exe /PID $procId /T /F | Out-Null } catch { }
}

function Stop-Own([System.Diagnostics.Process]$p) {
    if ($null -eq $p) { return }
    try {
        if (-not $p.HasExited) { $p.Kill() }
        $p.WaitForExit(5000) | Out-Null
    } catch { }
}

function Finish-One($it, [long]$tms) {
    if (-not $it.exited -and $it.p.HasExited) {
        $it.exitMs = $tms
        try { $it.rc = $it.p.ExitCode } catch { $it.rc = $null }
        $it.exited = $true
    }
    Drain-One $it $tms
    if (-not $it.closed) {
        try { $it.outW.Dispose() } catch { }
        try { $it.errW.Dispose() } catch { }
        $it.closed = $true
    }
}

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
            if (-not $p.WaitForExit(10000)) { Stop-Tree $p.Id }
        }
    }
}

function Run-Round([string]$name) {
    $roundDir = Join-Path $labelDir $name
    if (Test-Path -LiteralPath $roundDir) { throw "каталог прогона уже существует: $roundDir" }
    New-Item -ItemType Directory -Path $roundDir | Out-Null

    $mon = $null
    if (-not $NoMonitor) {
        $mon = Start-Monitors $roundDir
        Start-Sleep -Milliseconds 500
    }

    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $absStart = [DateTimeOffset]::UtcNow
    $items = New-Object System.Collections.ArrayList

    if ($Sequential) {
        for ($i = 0; $i -lt $Processes; $i++) {
            $it = Start-One $roundDir $i
            $it.startMs = $sw.ElapsedMilliseconds
            [void]$items.Add($it)
            while (-not $it.p.HasExited) {
                Pump-One $it $sw.ElapsedMilliseconds
                if ($sw.Elapsed.TotalSeconds -gt $TimeoutSec) {
                    $it.timedOut = $true
                    Stop-Own $it.p
                    break
                }
                Start-Sleep -Milliseconds 25
            }
            Finish-One $it $sw.ElapsedMilliseconds
        }
    } else {
        for ($i = 0; $i -lt $Processes; $i++) {
            $it = Start-One $roundDir $i
            $it.startMs = $sw.ElapsedMilliseconds
            [void]$items.Add($it)
        }
        while ($true) {
            $alive = 0
            foreach ($it in $items) {
                if (-not $it.exited) {
                    Pump-One $it $sw.ElapsedMilliseconds
                    if ($it.p.HasExited) {
                        $it.exitMs = $sw.ElapsedMilliseconds
                        try { $it.rc = $it.p.ExitCode } catch { $it.rc = $null }
                        $it.exited = $true
                    } else {
                        $alive++
                    }
                }
            }
            if ($alive -eq 0) { break }
            if ($sw.Elapsed.TotalSeconds -gt $TimeoutSec) {
                foreach ($it in $items) {
                    if (-not $it.exited) { $it.timedOut = $true; Stop-Own $it.p }
                }
                break
            }
            Start-Sleep -Milliseconds 25
        }
        foreach ($it in $items) { Finish-One $it $sw.ElapsedMilliseconds }
    }

    $endMs = $sw.ElapsedMilliseconds
    $absEnd = [DateTimeOffset]::UtcNow
    if (-not $NoMonitor) { Stop-Monitors $mon }

    $procs = @()
    foreach ($it in $items) {
        $sha = $null; $bytes = $null
        if (Test-Path -LiteralPath $it.wav) {
            $sha = (Get-FileHash -LiteralPath $it.wav -Algorithm SHA256).Hash.ToLower()
            $bytes = (Get-Item -LiteralPath $it.wav).Length
        }
        $procs += [ordered]@{
            index = $it.i; pid = $it.pid; start_ms = $it.startMs; exit_ms = $it.exitMs
            rc = $it.rc; timed_out = $it.timedOut
            wav = $it.wav; wav_sha256 = $sha; wav_bytes = $bytes
            stdout = $it.outLog; stderr = $it.errLog
            events = @($it.events)
        }
    }
    $doc = [ordered]@{
        label = $Label; round = $name; processes = $Processes; workers = $Workers
        sequential = [bool]$Sequential; no_monitor = [bool]$NoMonitor
        round_start_utc = $absStart.ToString('o')
        round_end_utc = $absEnd.ToString('o')
        round_start_epoch_ms = $absStart.ToUnixTimeMilliseconds()
        round_end_epoch_ms = $absEnd.ToUnixTimeMilliseconds()
        duration_ms = $endMs
        procs = $procs
    }
    $json = $doc | ConvertTo-Json -Depth 12
    [System.IO.File]::WriteAllText((Join-Path $roundDir 'run.json'), $json, $utf8NoBom)

    $brief = @()
    foreach ($p in $procs) { $brief += ("p{0}=rc{1}/{2}ms" -f $p.index, $p.rc, $p.exit_ms) }
    Write-Output ("=== {0}/{1}: {2}; group {3} ms; dir {4}" -f $Label, $name, ($brief -join ' '), $endMs, $roundDir)
}

$rounds = @()
if (-not $SkipWarmup) { $rounds += 'warmup' }
for ($r = 1; $r -le $Reps; $r++) { $rounds += ("r{0}" -f $r) }
foreach ($name in $rounds) { Run-Round $name }
Write-Output "готово: $labelDir"

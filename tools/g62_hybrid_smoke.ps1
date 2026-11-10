# G6.2 smoke: hybrid pool 2x2 through the real EXE (fake engine, no CUDA).
# Checks:
#   1. `--internal-batch-worker` without a value / with a typo / mixed with
#      another command -> exit 2, no window (fast self-exit).
#   2. `--help` lists the internal worker mode.
#   3. `--portable-selftest-hybrid` runs a REAL full hybrid batch: 2 child
#      processes, 4 workers, Named Pipe IPC, 6/6 files published (fake engine
#      writes real WAVs through the real sinks; ONNX/CUDA are not touched).
#   4. No child shtorm-gui processes are left behind (count before/after).
# Isolation: sandbox with its own copy of the exe and settings; only our own
# processes are started/stopped (by PID).
$ErrorActionPreference = 'Stop'
$exeSrc = 'C:\PocketTTS-GUI-Rust\ShtormTTS-Windows\target\release\shtorm-gui.exe'
$sandbox = Join-Path $env:TEMP ("shtorm_g62_smoke_" + $PID + "_" + (Get-Date -Format 'yyyyMMdd-HHmmss'))
New-Item -ItemType Directory -Path $sandbox | Out-Null
$exe = Join-Path $sandbox 'shtorm-gui.exe'
Copy-Item -LiteralPath $exeSrc -Destination $exe
$voice = 'C:/PocketTTS-GUI/examples/Microsoft Brian Speed 1.wav'
$onnxBase = 'C:/Users/alexa/Downloads/pocket_tts_android/build/onnx'
$outDir = ($sandbox -replace '\\', '/') + '/out'
$settingsJson = @"
{
  "text_file": "C:/nonexistent-placeholder.txt",
  "batch": true,
  "voice_wav": "$voice",
  "model": "Slow",
  "onnx_base": "$onnxBase",
  "device": "Cuda",
  "threads": 4,
  "seed": 42,
  "temperature": 0.3,
  "decode_steps": 1,
  "eos_threshold": -4.0,
  "eos_tail_short": 5,
  "eos_tail_long": 3,
  "brightness_db": 8.0,
  "denoise": false,
  "anti_repeat": true,
  "max_tokens": 40,
  "format": "Wav",
  "out_dir": "$outDir",
  "overwrite_existing": false,
  "batch_cuda_mode": "MaxSpeed",
  "theme_dark": true
}
"@
[IO.File]::WriteAllText((Join-Path $sandbox 'shtorm_gui_settings.json'), $settingsJson)
Write-Output ("SANDBOX: {0}" -f $sandbox)

function Get-WorkerCount {
    $procs = Get-Process -Name 'shtorm-gui' -ErrorAction SilentlyContinue
    return @($procs).Count
}

$script:failures = 0
function Run-Case {
    param([string]$name, [string[]]$argv, [int]$expectCode, [string]$expectOut, [double]$maxSec)
    $t0 = Get-Date
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $exe
    $psi.Arguments = ($argv | ForEach-Object { $_ }) -join ' '
    $psi.WorkingDirectory = $sandbox
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.StandardOutputEncoding = [Text.Encoding]::UTF8
    $psi.StandardErrorEncoding = [Text.Encoding]::UTF8
    $p = [System.Diagnostics.Process]::Start($psi)
    $outTask = $p.StandardOutput.ReadToEndAsync()
    $errTask = $p.StandardError.ReadToEndAsync()
    $exited = $p.WaitForExit(120000)
    if ($exited) { $null = $p.WaitForExit() }
    if (-not $exited) {
        Stop-Process -Id $p.Id -Force
        Write-Output ("FAIL {0}: did not exit within 120 s" -f $name)
        $script:failures++
        return
    }
    $dur = ((Get-Date) - $t0).TotalSeconds
    $out = $outTask.Result
    $err = $errTask.Result
    $ok = $true
    if ($p.ExitCode -ne $expectCode) { Write-Output ("FAIL {0}: ExitCode {1} != {2}" -f $name, $p.ExitCode, $expectCode); $ok = $false }
    if ($dur -gt $maxSec) { Write-Output ("FAIL {0}: duration {1:N1} s > {2:N1} s" -f $name, $dur, $maxSec); $ok = $false }
    if ($expectOut -and -not (("`n" + $out + "`n" + $err + "`n").Contains($expectOut))) {
        Write-Output ("FAIL {0}: output does not contain '{1}'; stdout={2}; stderr={3}" -f $name, $expectOut, $out, $err); $ok = $false }
    if ($ok) { Write-Output ("PASS {0} (exit {1}, {2:N1} s)" -f $name, $p.ExitCode, $dur) } else { $script:failures++ }
}

$workersBefore = Get-WorkerCount

# --- internal worker flag: bad usage must never open a window ---
Run-Case 'worker-flag-without-value' @('--internal-batch-worker') 2 'значение' 5
Run-Case 'worker-flag-typo'         @('--internal-batch-worker-typo') 2 'неизвестный аргумент' 5
Run-Case 'worker-flag-value-is-flag' @('--internal-batch-worker', '--portable-check') 2 'не имя канала' 5
Run-Case 'help-lists-worker'        @('--help') 0 '--internal-batch-worker' 5

# --- real hybrid batch on the fake engine: 2 processes x 2 workers ---
Run-Case 'hybrid-selftest' @('--portable-selftest-hybrid') 0 'HYBRID SELFTEST OK' 120
# explicit checks of the counters in the last run's output are inside Run-Case;
# a second targeted check: run once more capturing output for counters
$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $exe
$psi.Arguments = '--portable-selftest-hybrid'
$psi.WorkingDirectory = $sandbox
$psi.UseShellExecute = $false
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.StandardOutputEncoding = [Text.Encoding]::UTF8
$psi.StandardErrorEncoding = [Text.Encoding]::UTF8
$p = [System.Diagnostics.Process]::Start($psi)
$out = $p.StandardOutput.ReadToEnd()
$err = $p.StandardError.ReadToEnd()
$exited = $p.WaitForExit(120000)
if (-not $exited) { Stop-Process -Id $p.Id -Force; Write-Output 'FAIL hybrid-counters: timeout'; $script:failures++ }
elseif ($out -notmatch 'процессов 2, воркеров 4, готово 6/6') {
    Write-Output ("FAIL hybrid-counters: {0}" -f $out); $script:failures++
} else { Write-Output 'PASS hybrid-counters (2 процесса, 4 воркера, 6/6 файлов)' }

# --- no leftover child processes ---
Start-Sleep -Milliseconds 500
$workersAfter = Get-WorkerCount
if ($workersAfter -gt $workersBefore) {
    Write-Output ("FAIL leftover-processes: было {0}, стало {1}" -f $workersBefore, $workersAfter)
    $script:failures++
} else {
    Write-Output ("PASS leftover-processes (было {0}, стало {1})" -f $workersBefore, $workersAfter)
}

Write-Output ("HYBRID SMOKE G6.2: {0}" -f $(if ($script:failures -eq 0) { 'ALL PASS' } else { "$($script:failures) FAILURE(S)" }))
Write-Output ("SANDBOX (artifacts stay): {0}" -f $sandbox)
exit $(if ($script:failures -eq 0) { 0 } else { 1 })

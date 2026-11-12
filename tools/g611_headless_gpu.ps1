# G6.1.1 headless GPU acceptance: real CUDA synthesis through the EXE
# diagnostic modes, in the same isolated sandbox style as the CLI smoke.
$ErrorActionPreference = 'Stop'
$exeSrc = 'C:\PocketTTS-GUI-Rust\ShtormTTS-Windows\target\release\PocketTTS-GUI-Rust.exe'
$sandbox = Join-Path $env:TEMP ("pockettts_g611_headless_" + $PID + "_" + (Get-Date -Format 'yyyyMMdd-HHmmss'))
New-Item -ItemType Directory -Path $sandbox | Out-Null
$exe = Join-Path $sandbox 'PocketTTS-GUI-Rust.exe'
Copy-Item -LiteralPath $exeSrc -Destination $exe
$voice = 'C:/PocketTTS-GUI/examples/Василий Леванов F5-TTS3.wav'
$onnxBase = 'C:/Users/<user>/Downloads/pocket_tts_android/build/onnx'
$gpuDll = 'C:/PocketTTS-GUI-Rust/ShtormTTS-Windows/deps/portable/onnxruntime.dll'
$outDir = ($sandbox -replace '\\', '/') + '/out'
$settingsJson = @"
{
  "text_file": "C:/nonexistent-placeholder.txt",
  "batch": false,
  "voice_wav": "$voice",
  "model": "Slow",
  "onnx_base": "$onnxBase",
  "device": "Cuda",
  "ort_gpu_dll": "$gpuDll",
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
  "pause_scale": 1.0,
  "paragraph_pause_s": 0.9,
  "max_tokens": 50,
  "format": "Wav",
  "out_dir": "$outDir",
  "overwrite_existing": false,
  "theme_dark": true
}
"@
[IO.File]::WriteAllText((Join-Path $sandbox 'PocketTTS-GUI-Rust.json'), $settingsJson)
Write-Output ("SANDBOX: {0}" -f $sandbox)

$script:failures = 0
function Run-Mode {
    param([string]$name, [string[]]$argv, [string[]]$expect, [int]$expectCode)
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $exe
    $psi.Arguments = ($argv -join ' ')
    $psi.WorkingDirectory = $sandbox
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.StandardOutputEncoding = [Text.Encoding]::UTF8
    $psi.StandardErrorEncoding = [Text.Encoding]::UTF8
    $p = [System.Diagnostics.Process]::Start($psi)
    $outTask = $p.StandardOutput.ReadToEndAsync()
    $errTask = $p.StandardError.ReadToEndAsync()
    $exited = $p.WaitForExit(300000)   # real synthesis: up to 5 min
    if ($exited) { $null = $p.WaitForExit() }
    if (-not $exited) {
        Stop-Process -Id $p.Id -Force
        Write-Output ("FAIL {0}: did not exit within 300 s" -f $name); $script:failures++; return
    }
    $out = $outTask.Result
    $err = $errTask.Result
    $ok = $true
    if ($p.ExitCode -ne $expectCode) { Write-Output ("FAIL {0}: ExitCode {1} != {2}" -f $name, $p.ExitCode, $expectCode); $ok = $false }
    foreach ($e in $expect) {
        if (-not (($out + "`n" + $err).Contains($e))) { Write-Output ("FAIL {0}: output does not contain '{1}'" -f $name, $e); $ok = $false }
    }
    if ($ok) { Write-Output ("PASS {0} (exit {1})" -f $name, $p.ExitCode) } else { $script:failures++; Write-Output $out }
}

Run-Mode 'selftest (single CUDA WAV)' @('--portable-selftest') @('STATUS: Завершено', 'io binding active') 0
Run-Mode 'selftest-batch (CUDA pool)' @('--portable-selftest-batch') @('STATUS: Завершено', 'Pocket загружен 2 раз') 0
Run-Mode 'selftest-cancel (CUDA)' @('--portable-selftest-cancel') @('CANCEL SELFTEST OK') 0

# orphaned ffmpeg after all modes (WAV only -> expected 0 anyway)
$ff = (Get-Process -Name ffmpeg -ErrorAction SilentlyContinue | Measure-Object).Count
Write-Output ("orphan ffmpeg processes: {0}" -f $ff)
if ($ff -ne 0) { Write-Output 'FAIL: orphaned ffmpeg'; $script:failures++ }

Write-Output ("HEADLESS GPU: {0}" -f $(if ($script:failures -eq 0) { 'ALL PASS' } else { "$($script:failures) FAILURE(S)" }))
exit $(if ($script:failures -eq 0) { 0 } else { 1 })

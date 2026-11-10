# G6.1.2 smoke: repeated CLI options are rejected by the real EXE.
# The parser must reject a second --device/--format/--hold-ms regardless of
# value equality (exit 2, message names the option, GUI does NOT open —
# the process exits by itself within seconds; a live eframe window blocks
# exit). Valid option orders and the plain no-args GUI launch are regression
# checks. Everything runs in an isolated sandbox with our own PIDs only.
$ErrorActionPreference = 'Stop'
$exeSrc = 'C:\PocketTTS-GUI-Rust\ShtormTTS-Windows\target\release\shtorm-gui.exe'
$sandbox = Join-Path $env:TEMP ("shtorm_g612_smoke_" + $PID + "_" + (Get-Date -Format 'yyyyMMdd-HHmmss'))
New-Item -ItemType Directory -Path $sandbox | Out-Null
$exe = Join-Path $sandbox 'shtorm-gui.exe'
Copy-Item -LiteralPath $exeSrc -Destination $exe
$voice = 'C:/PocketTTS-GUI/examples/Microsoft Brian Speed 1.wav'
$onnxBase = 'C:/Users/alexa/Downloads/pocket_tts_android/build/onnx'
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
  "max_tokens": 40,
  "format": "Wav",
  "out_dir": "$outDir",
  "overwrite_existing": false,
  "theme_dark": true
}
"@
[IO.File]::WriteAllText((Join-Path $sandbox 'shtorm_gui_settings.json'), $settingsJson)
Write-Output ("SANDBOX: {0}" -f $sandbox)

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
    $exited = $p.WaitForExit(15000)
    if ($exited) { $null = $p.WaitForExit() }
    if (-not $exited) {
        Stop-Process -Id $p.Id -Force
        Write-Output ("FAIL {0}: did not exit within 15 s (GUI opened?)" -f $name)
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

# --- G6.1.2: repeated options must be rejected (code 2, no window) ---
Run-Case 'dup-device-diff-values'  @('--portable-check', '--device', 'cpu', '--device', 'cuda') 2 'дважды' 5
Run-Case 'dup-device-same-value'   @('--portable-check', '--device', 'cuda', '--device', 'cuda') 2 'дважды' 5
Run-Case 'dup-device-around-cmd'   @('--device', 'cpu', '--portable-check', '--device', 'cuda') 2 'дважды' 5
Run-Case 'dup-format'              @('--portable-check', '--format', 'wav', '--format', 'mp3') 2 'дважды' 5
Run-Case 'dup-format-same-value'   @('--portable-check', '--format', 'wav', '--format', 'wav') 2 'дважды' 5
Run-Case 'dup-hold-ms'             @('--portable-check', '--hold-ms', '10', '--hold-ms', '20') 2 'дважды' 5

# --- regressions of the strict parser (G6.1.1 table, subset) ---
Run-Case 'unknown-argument'        @('--definitely-not-a-flag') 2 'ОШИБКА АРГУМЕНТОВ' 5
Run-Case 'missing-value'           @('--portable-check', '--device') 2 'значение' 5

# --- valid: options in different orders still work ---
Run-Case 'options-after-command'   @('--portable-check', '--device', 'cuda') 0 'PORTABLE CHECK OK' 15
Run-Case 'options-before-command'  @('--device', 'cuda', '--portable-check') 0 'PORTABLE CHECK OK' 15
Run-Case 'options-reordered'       @('--hold-ms', '0', '--format', 'wav', '--device', 'cuda', '--portable-check') 0 'PORTABLE CHECK OK' 15

# --- correct normal launch: no args, GUI must open and stay alive ---
$t0 = Get-Date
$gui = Start-Process -FilePath $exe -WorkingDirectory $sandbox -PassThru -WindowStyle Minimized
Start-Sleep -Seconds 4
$alive = -not $gui.HasExited
if ($alive) {
    $null = $gui.CloseMainWindow()
    if (-not $gui.WaitForExit(5000)) { Stop-Process -Id $gui.Id -Force; $gui.WaitForExit() }
    Write-Output ("PASS gui-no-args: alive >= 4 s (window opened), closed in {0:N1} s, exit {1}" -f ((Get-Date) - $t0).TotalSeconds, $gui.ExitCode)
} else {
    Write-Output ("FAIL gui-no-args: exited by itself in {0:N1} s with code {1} - GUI did not open" -f ((Get-Date) - $t0).TotalSeconds, $gui.ExitCode)
    $script:failures++
}

Write-Output ("CLI SMOKE G6.1.2: {0}" -f $(if ($script:failures -eq 0) { 'ALL PASS' } else { "$($script:failures) FAILURE(S)" }))
Write-Output ("SANDBOX (artifacts stay): {0}" -f $sandbox)
exit $(if ($script:failures -eq 0) { 0 } else { 1 })

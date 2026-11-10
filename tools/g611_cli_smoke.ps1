# G6.1.1 smoke: CLI-dispatch of the real EXE in an isolated sandbox.
# Checks ExitCode/stdout/stderr and that bad args do NOT open the GUI
# (the process exits by itself within seconds; a live eframe window blocks
# exit). The no-args GUI mode is checked with our own PID: the process must
# stay alive >= 4 s (window opened), then closed softly (CloseMainWindow)
# or stopped by PID - our own test process only.
$ErrorActionPreference = 'Stop'
$exeSrc = 'C:\PocketTTS-GUI-Rust\ShtormTTS-Windows\target\release\shtorm-gui.exe'
$sandbox = Join-Path $env:TEMP ("shtorm_g611_smoke_" + $PID + "_" + (Get-Date -Format 'yyyyMMdd-HHmmss'))
New-Item -ItemType Directory -Path $sandbox | Out-Null
$exe = Join-Path $sandbox 'shtorm-gui.exe'
Copy-Item -LiteralPath $exeSrc -Destination $exe
$voice = 'C:/PocketTTS-GUI/examples/Microsoft Brian Speed 1.wav'
$onnxBase = 'C:/Users/alexa/Downloads/pocket_tts_android/build/onnx'
$gpuDll = 'C:/PocketTTS-GUI-Rust/ShtormTTS-Windows/deps/portable/onnxruntime.dll'
$outDir = ($sandbox -replace '\\', '/') + '/out'
# JSON paths use forward slashes: valid JSON, accepted by Windows/PathBuf.
# The app writes JSON without BOM; WriteAllText(default) = UTF-8 no BOM.
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
    # the exe writes UTF-8 (Rust); decode the pipes as UTF-8, not OEM
    $psi.StandardOutputEncoding = [Text.Encoding]::UTF8
    $psi.StandardErrorEncoding = [Text.Encoding]::UTF8
    $p = [System.Diagnostics.Process]::Start($psi)
    # async reads first: avoids pipe deadlock if output ever exceeds the buffer
    $outTask = $p.StandardOutput.ReadToEndAsync()
    $errTask = $p.StandardError.ReadToEndAsync()
    $exited = $p.WaitForExit(15000)
    if ($exited) { $null = $p.WaitForExit() }   # sync reap for a reliable ExitCode
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
        Write-Output ("FAIL {0}: output does not contain '{1}'; stdout={2}; stderr={3}" -f $name, $expectOut, $out, $err); $ok = $false
    }
    if ($ok) { Write-Output ("PASS {0} (exit {1}, {2:N1} s)" -f $name, $p.ExitCode, $dur) } else { $script:failures++ }
}

# --- argument-error table (no GUI: fast self-exit, code 2) ---
Run-Case 'unknown-flag-typo (G6.1 incident)' @('--portable-batch-selftest') 2 'ОШИБКА АРГУМЕНТОВ' 5
Run-Case 'unknown-option'          @('--unknown-option') 2 'ОШИБКА АРГУМЕНТОВ' 5
Run-Case 'unknown-after-valid'     @('--portable-check', '--unknown-option') 2 'ОШИБКА АРГУМЕНТОВ' 5
Run-Case 'missing-value'           @('--portable-check', '--device') 2 'значение' 5
Run-Case 'two-commands'            @('--portable-check', '--portable-selftest') 2 'несовместимы' 5
Run-Case 'bad-device-value'        @('--portable-check', '--device', 'gpu') 2 'недопустимое значение' 5
Run-Case 'bad-format-value'        @('--portable-check', '--format', 'flac') 2 'недопустимое значение' 5
Run-Case 'bad-holdms-value'        @('--portable-check', '--hold-ms', 'abc') 2 'недопустимое значение' 5
Run-Case 'option-without-command'  @('--device', 'cuda') 2 'только вместе с диагностической' 5

# --- valid headless modes that load no Pocket ---
Run-Case 'help'                    @('--help') 0 'ShtormTTS GUI' 5
Run-Case 'help-short'              @('-h') 0 'ShtormTTS GUI' 5
Run-Case 'portable-check'          @('--portable-check') 0 'PORTABLE CHECK OK' 15
Run-Case 'options-before-command'  @('--device', 'cuda', '--portable-check') 0 'PORTABLE CHECK OK' 15

# --- no-args GUI: the process must STAY ALIVE (a live eframe window blocks exit) ---
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

Write-Output ("CLI SMOKE: {0}" -f $(if ($script:failures -eq 0) { 'ALL PASS' } else { "$($script:failures) FAILURE(S)" }))
Write-Output ("SANDBOX (artifacts stay): {0}" -f $sandbox)
exit $(if ($script:failures -eq 0) { 0 } else { 1 })

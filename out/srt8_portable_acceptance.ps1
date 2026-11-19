# SRT.8 portable acceptance: SRT EXEs in a hardlink sandbox built from an
# existing portable kit (full kit is NOT rebuilt here). ASCII-only output
# (PS 5.1 reads no-BOM UTF-8 as ANSI; Cyrillic literals break the parser).
param(
    [string]$Kit = "C:\PocketTTS-GUI-Rust\dist\PocketTTS-GUI-Rust-Portable-2026-11-14-g7",
    [string]$Repo = "C:\PocketTTS-GUI-Rust\ShtormTTS-Windows"
)
$ErrorActionPreference = "Stop"

$sandbox = Join-Path $env:TEMP ("srt8_kit_" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $sandbox | Out-Null
Write-Output "SANDBOX: $sandbox"

$count = 0
Get-ChildItem -LiteralPath $Kit -Recurse -File | ForEach-Object {
    $rel = $_.FullName.Substring($Kit.Length + 1)
    $target = Join-Path $sandbox $rel
    $dir = Split-Path -Parent $target
    if (-not (Test-Path -LiteralPath $dir)) { New-Item -ItemType Directory -Path $dir -Force | Out-Null }
    $ok = New-Item -ItemType HardLink -Path $target -Value $_.FullName -ErrorAction SilentlyContinue
    if (-not $ok) { Copy-Item -LiteralPath $_.FullName -Destination $target }
    $count++
}
Write-Output "HARDLINKS: $count files from kit"

Copy-Item "$Repo\target\release\PocketTTS-GUI-Rust-srt.exe" $sandbox
Copy-Item "$Repo\target\release\PocketTTS-GUI-Rust-srt-cpu.exe" $sandbox
Copy-Item "$Repo\third_party\soundtouch\SoundTouchDLL.dll" $sandbox
New-Item -ItemType Directory -Path (Join-Path $sandbox "LICENSES") -Force | Out-Null
Copy-Item "$Repo\third_party\soundtouch\LGPL-2.1.txt" (Join-Path $sandbox "LICENSES\SoundTouch-LGPL-2.1.txt")
Copy-Item "$Repo\third_party\soundtouch\NOTICE.txt" (Join-Path $sandbox "LICENSES\SoundTouch-NOTICE.txt")

$mainConf = Join-Path $sandbox "PocketTTS-GUI-Rust.json"
$mainConfHashBefore = (Get-FileHash -LiteralPath $mainConf -Algorithm SHA256).Hash

# GUI-subsystem exe: stdout/stderr only via explicit redirection (as g62).
function Run-Captured {
    param([string]$exe, [string]$argLine)
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $exe
    $psi.Arguments = $argLine
    $psi.WorkingDirectory = "C:\"
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.StandardOutputEncoding = [Text.Encoding]::UTF8
    $psi.StandardErrorEncoding = [Text.Encoding]::UTF8
    $p = [System.Diagnostics.Process]::Start($psi)
    $outTask = $p.StandardOutput.ReadToEndAsync()
    $errTask = $p.StandardError.ReadToEndAsync()
    if (-not $p.WaitForExit(120000)) { Stop-Process -Id $p.Id -Force; throw "no exit in 120 s: $exe" }
    $null = $p.WaitForExit()
    [pscustomobject]@{ Code = $p.ExitCode; Out = $outTask.Result; Err = $errTask.Result }
}

# 1) portable-check: both SRT EXEs, each must read the SRT config file.
foreach ($exeName in @("PocketTTS-GUI-Rust-srt.exe", "PocketTTS-GUI-Rust-srt-cpu.exe")) {
    $r = Run-Captured (Join-Path $sandbox $exeName) "--portable-check"
    foreach ($line in ($r.Out -split "`r?`n")) {
        if ($line -match '^(EXE|SETTINGS|MODEL|ORT DLL|FFMPEG|PORTABLE)') { Write-Output "CHECK[$exeName]: $line" }
    }
    if ($r.Code -ne 0 -or -not $r.Out.Contains("PORTABLE CHECK OK")) {
        Write-Output "ACCEPT FAILED: portable-check $exeName (exit $($r.Code))"; exit 1
    }
    if (-not $r.Out.Contains("PocketTTS-GUI-Rust-srt.json")) {
        Write-Output "ACCEPT FAILED: $exeName does not use the SRT config"; exit 1
    }
}
Write-Output "ACCEPT portable-check OK (both SRT EXEs, SRT config)"

# 2) GUI smoke: window opens, lives 4 s, closes cleanly (exit 0).
foreach ($exe in @("PocketTTS-GUI-Rust-srt.exe", "PocketTTS-GUI-Rust-srt-cpu.exe")) {
    $p = Start-Process -FilePath (Join-Path $sandbox $exe) -WorkingDirectory "C:\" -PassThru
    Start-Sleep -Seconds 4
    if ($p.HasExited) {
        Write-Output "ACCEPT FAILED: $exe exited within 4 s (code $($p.ExitCode))"; exit 1
    }
    $null = $p.CloseMainWindow()
    if (-not $p.WaitForExit(20000)) {
        $p.Kill(); Write-Output "ACCEPT FAILED: $exe did not close in 20 s"; exit 1
    }
    if ($p.ExitCode -ne 0) {
        Write-Output "ACCEPT FAILED: $exe exit code $($p.ExitCode)"; exit 1
    }
    Write-Output "ACCEPT gui-smoke OK: $exe (exit 0)"
}

# 3) FirstRun isolation: SRT config created; the Text config untouched.
$srtConf = Join-Path $sandbox "PocketTTS-GUI-Rust-srt.json"
if (-not (Test-Path -LiteralPath $srtConf)) {
    Write-Output "ACCEPT FAILED: SRT config not created on FirstRun"; exit 1
}
$mainConfHashAfter = (Get-FileHash -LiteralPath $mainConf -Algorithm SHA256).Hash
if ($mainConfHashBefore -ne $mainConfHashAfter) {
    Write-Output "ACCEPT FAILED: Text config modified by SRT version"; exit 1
}
Write-Output "ACCEPT config isolation OK (srt.json created, PocketTTS-GUI-Rust.json intact)"

# 4) no orphan ffmpeg/worker processes
$ff = Get-Process ffmpeg -ErrorAction SilentlyContinue
if ($ff) { Write-Output "ACCEPT FAILED: live ffmpeg: $($ff.Count)"; exit 1 }
Write-Output "ACCEPT orphans 0"

Write-Output "SRT8 PORTABLE ACCEPTANCE ALL PASS"
Write-Output "SANDBOX_LEFT: $sandbox"
exit 0

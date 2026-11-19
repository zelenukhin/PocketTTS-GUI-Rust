# SRT.9D acceptance add-on: normal EXEs (Text flavor) must work in a
# portable layout WITHOUT SoundTouchDLL.dll (they never load it).
# ASCII-only output for PS 5.1.
param(
    [string]$Kit = "C:\PocketTTS-GUI-Rust\dist\PocketTTS-GUI-Rust-Portable-2026-11-14-g7",
    [string]$Repo = "C:\PocketTTS-GUI-Rust\ShtormTTS-Windows"
)
$ErrorActionPreference = "Stop"

$sandbox = Join-Path $env:TEMP ("srt9_nodll_" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $sandbox | Out-Null

# Kit payload WITHOUT SoundTouchDLL.dll (kit g7 predates SRT and has none,
# but copy defensively with an exclusion so the point is proven).
Get-ChildItem -LiteralPath $Kit -Recurse -File | Where-Object { $_.Name -ne "SoundTouchDLL.dll" } | ForEach-Object {
    $rel = $_.FullName.Substring($Kit.Length + 1)
    $target = Join-Path $sandbox $rel
    $dir = Split-Path -Parent $target
    if (-not (Test-Path -LiteralPath $dir)) { New-Item -ItemType Directory -Path $dir -Force | Out-Null }
    $ok = New-Item -ItemType HardLink -Path $target -Value $_.FullName -ErrorAction SilentlyContinue
    if (-not $ok) { Copy-Item -LiteralPath $_.FullName -Destination $target }
}
if (Test-Path (Join-Path $sandbox "SoundTouchDLL.dll")) { throw "sandbox must not contain SoundTouchDLL.dll" }
Write-Output "SANDBOX (no SoundTouchDLL): $sandbox"

foreach ($exe in @("PocketTTS-GUI-Rust.exe", "PocketTTS-GUI-Rust-cpu.exe")) {
    $p = Start-Process -FilePath (Join-Path $sandbox $exe) -ArgumentList "--portable-check" `
        -NoNewWindow -Wait -PassThru -RedirectStandardOutput "$env:TEMP\srt9_check_out.txt" -RedirectStandardError "$env:TEMP\srt9_check_err.txt"
    $out = Get-Content "$env:TEMP\srt9_check_out.txt" -Raw
    if ($p.ExitCode -ne 0 -or -not $out.Contains("PORTABLE CHECK OK")) {
        throw "portable-check failed for $exe (rc=$($p.ExitCode))"
    }
    Write-Output "CHECK[$exe]: PORTABLE CHECK OK (no SoundTouchDLL present)"
}

# Real CPU TXT selftest with the normal (glow) EXE — full synthesis path
# works without the DLL (TXT flavor never loads SoundTouch).
$p = Start-Process -FilePath (Join-Path $sandbox "PocketTTS-GUI-Rust.exe") -ArgumentList "--portable-selftest" `
    -NoNewWindow -Wait -PassThru -RedirectStandardOutput "$env:TEMP\srt9_st_out.txt" -RedirectStandardError "$env:TEMP\srt9_st_err.txt"
$out = Get-Content "$env:TEMP\srt9_st_out.txt" -Raw
if ($p.ExitCode -ne 0) { throw "selftest rc=$($p.ExitCode): $out" }
if (-not $out.Contains("OUTPUT:")) { throw "selftest marker missing: $out" }
Write-Output "CHECK[PocketTTS-GUI-Rust.exe]: PORTABLE SELFTEST OK (real CPU synth, no SoundTouchDLL)"

# Orphan check: no leftover ffmpeg/worker processes from this sandbox.
$orphans = Get-Process | Where-Object { $_.Path -like "$sandbox*" } | Measure-Object | Select-Object -ExpandProperty Count
if ($orphans -ne 0) { throw "orphan processes: $orphans" }
Write-Output "ACCEPT normal EXEs work without SoundTouchDLL (check + selftest + orphans 0)"
Write-Output "SRT9 NO-DLL ACCEPTANCE PASS"

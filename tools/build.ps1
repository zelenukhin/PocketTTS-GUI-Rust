# Сборка ShtormTTS-Windows (release) + юнит-тесты.
# Никаких системных изменений: только cargo в каталоге проекта.
param(
    [switch]$SkipTests
)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

Write-Host "== cargo build --release =="
cargo build --release
if ($LASTEXITCODE -ne 0) { throw "build failed" }

if (-not $SkipTests) {
    Write-Host "== cargo test =="
    cargo test
    if ($LASTEXITCODE -ne 0) { throw "tests failed" }
}

Write-Host ""
Write-Host "Готово: target\release\smoke.exe, target\release\bench.exe"
Write-Host "Смоук:  tools\smoke-cpu.ps1 / tools\smoke-cuda.ps1"

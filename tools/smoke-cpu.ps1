# Смоук-тест CPU FP32.
# Пример: tools\smoke-cpu.ps1 -ModelDir C:\...\onnx\ru-v2-slow -Voice C:\...\voice.wav
param(
    [Parameter(Mandatory)] [string]$ModelDir,
    [Parameter(Mandatory)] [string]$Voice,
    [string]$Text = "Стр+елки час+ов показ+ывали п+олночь, а стрелк+и заст+ыли на пост+ах.",
    [string]$Out = "out\smoke_cpu.wav",
    [int]$Threads = 4,
    [int]$Repeats = 2,
    [double]$Temperature = 0.3,
    [int]$DecodeSteps = 1,
    [long]$Seed = 42,
    [double]$BrightnessDb = 8.0,
    [switch]$Denoise
)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

# Каталог onnxruntime DLL внутри проекта; при отсутствии — подсказка.
$ortDll = Join-Path $root "deps\ort-cpu\onnxruntime-win-x64-1.23.2\lib\onnxruntime.dll"
if (-not (Test-Path $ortDll)) {
    throw "onnxruntime.dll не найден: $ortDll`nРаспакуйте onnxruntime-win-x64-1.23.2.zip в deps\ort-cpu\ (см. docs\PREPARATION_REPORT.md)."
}

$env:ORT_DYLIB_PATH = $ortDll
New-Item -ItemType Directory -Force -Path (Split-Path -Parent $Out) | Out-Null

$extra = @()
if ($Denoise) { $extra += "--denoise" }

& "$root\target\release\smoke.exe" `
    --model-dir $ModelDir --voice $Voice --text $Text --out $Out `
    --device cpu --threads $Threads --repeats $Repeats `
    --temperature $Temperature --decode-steps $DecodeSteps --seed $Seed `
    --brightness-db $BrightnessDb @extra
exit $LASTEXITCODE

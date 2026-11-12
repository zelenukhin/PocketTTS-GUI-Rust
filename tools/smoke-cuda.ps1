# Смоук-тест CUDA FP32.
# Требуется GPU-сборка onnxruntime (deps\ort-gpu-wheel\x\onnxruntime\capi\onnxruntime.dll)
# и DLL CUDA 12 + cuDNN 9, доступные через PATH. По умолчанию скрипт добавляет в PATH
# (только для дочернего процесса, система не меняется):
#   1) deps\ort-gpu-wheel\x\onnxruntime\capi — onnxruntime_providers_cuda.dll
#   2) torch-venv lib — cudart64_12, cublas64_12, cudnn64_9, cufft64_11 (прокси-источник)
# Пример: tools\smoke-cuda.ps1 -ModelDir C:\...\onnx\ru-v2-slow -Voice C:\...\voice.wav
param(
    [Parameter(Mandatory)] [string]$ModelDir,
    [Parameter(Mandatory)] [string]$Voice,
    [string]$Text = "Стр+елки час+ов показ+ывали п+олночь, а стрелк+и заст+ыли на пост+ах.",
    [string]$Out = "out\smoke_cuda.wav",
    [int]$DeviceId = 0,
    [int]$Repeats = 2,
    [double]$Temperature = 0.3,
    [int]$DecodeSteps = 1,
    [long]$Seed = 42,
    [double]$BrightnessDb = 8.0,
    [string]$TorchLib = "C:\PocketTTS-GUI\venv\Lib\site-packages\torch\lib",
    [switch]$Denoise
)
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$capi = Join-Path $root "deps\ort-gpu-wheel\x\onnxruntime\capi"
$ortDll = Join-Path $capi "onnxruntime.dll"
if (-not (Test-Path $ortDll)) {
    throw "GPU-сборка onnxruntime.dll не найдена: $ortDll`nУстановите колесо onnxruntime-gpu==1.23.2 в deps\ort-gpu-wheel\ (см. docs\PREPARATION_REPORT.md)."
}
if (-not (Test-Path $TorchLib)) {
    Write-Warning "TorchLib не найден: $TorchLib — CUDA/cuDNN DLL придётся обеспечить иначе (установленный CUDA Toolkit 12 в PATH)."
}

# PATH — только для этого запуска; системный PATH не трогаем.
$env:PATH = "$capi;$TorchLib;$env:PATH"
$env:ORT_DYLIB_PATH = $ortDll
New-Item -ItemType Directory -Force -Path (Split-Path -Parent $Out) | Out-Null

$extra = @()
if ($Denoise) { $extra += "--denoise" }

& "$root\target\release\smoke.exe" `
    --model-dir $ModelDir --voice $Voice --text $Text --out $Out `
    --device cuda --device-id $DeviceId --repeats $Repeats `
    --temperature $Temperature --decode-steps $DecodeSteps --seed $Seed `
    --brightness-db $BrightnessDb @extra
exit $LASTEXITCODE

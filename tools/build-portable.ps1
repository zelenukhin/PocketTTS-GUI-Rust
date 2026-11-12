# Build a portable PocketTTS-GUI-Rust distribution for Windows x64.
#
# Safety policy:
#   * Only the project sources and the explicitly passed destination are touched.
#   * The destination must be a NEW directory OUTSIDE the source repository.
#   * No Remove-Item / rmdir / git clean / recursive deletion is used anywhere.
#   * If a check fails, the partially created destination is left in place for
#     inspection; do not reuse it, choose a new path.
#
# G6.5: the app is named PocketTTS-GUI-Rust; destination directories must use
# the new name (e.g. PocketTTS-GUI-Rust-Portable-<date>-<tag>). Destinations
# still carrying the old ShtormTTS-Portable name are rejected.
#
# Example:
#   powershell -ExecutionPolicy Bypass -File tools\build-portable.ps1 `
#     -Destination C:\PocketTTS-GUI-Rust\dist\PocketTTS-GUI-Rust-Portable-2026-11-12 `
#     -ModelsDir C:\Users\<user>\Downloads\pocket_tts_android\build\onnx `
#     -Ffmpeg C:\PocketTTS-GUI\ffmpeg\ffmpeg.exe `
#     -VoiceFile "C:\PocketTTS-GUI\examples\Microsoft Brian Speed 1.wav"

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Destination,
    [string]$ModelsDir = "",
    [string]$Ffmpeg = "",
    [string]$VoiceFile = "",
    [string]$VcRuntimeDir = "",
    [switch]$SkipModels,
    [switch]$SkipCuda,
    [switch]$SkipFfmpeg,
    [switch]$SkipInt8Models,
    [switch]$HardLinkPayload,
    [ValidateSet("release", "debug")][string]$Configuration = "release"
)

$ErrorActionPreference = "Stop"

$repo = [IO.Path]::GetFullPath((Split-Path -Parent $PSScriptRoot)).TrimEnd('\')
$dest = [IO.Path]::GetFullPath($Destination).TrimEnd('\')

if (Test-Path -LiteralPath $dest) {
    throw "Destination already exists (refusing to overwrite/clean): $dest"
}
# G6.5: не выпускать комплекты под прежним именем приложения
$destLeaf = Split-Path -Leaf $dest
if ($destLeaf -like '*ShtormTTS-Portable*') {
    throw "Destination uses the retired app name: $destLeaf (use PocketTTS-GUI-Rust-Portable-*)"
}
if ($dest -ieq $repo -or $dest.StartsWith($repo + '\', [System.StringComparison]::OrdinalIgnoreCase)) {
    throw "Destination must be OUTSIDE the source repository for a true portable test: $dest"
}
if ($repo.StartsWith($dest + '\', [System.StringComparison]::OrdinalIgnoreCase)) {
    throw "Destination must not be an ancestor of the source repository: $dest"
}
$rootOfDest = [IO.Path]::GetPathRoot($dest)
if ($dest.TrimEnd('\') -ieq $rootOfDest.TrimEnd('\')) {
    throw "Destination must not be a drive root: $dest"
}

$parent = Split-Path -Parent $dest
if (-not (Test-Path -LiteralPath $parent)) {
    New-Item -ItemType Directory -Path $parent -Force | Out-Null
}
New-Item -ItemType Directory -Path $dest | Out-Null

$templateDir = Join-Path $repo "tools\portable"
if (-not (Test-Path -LiteralPath $templateDir)) {
    throw "Portable templates missing: $templateDir"
}
$script:copied = New-Object System.Collections.ArrayList

function Ensure-Dir([string]$relative) {
    $p = Join-Path $dest $relative
    if (-not (Test-Path -LiteralPath $p)) {
        New-Item -ItemType Directory -Path $p | Out-Null
    }
}

function Write-Utf8NoBom([string]$path, [string]$text) {
    $enc = New-Object System.Text.UTF8Encoding($false)
    [System.IO.File]::WriteAllText($path, $text, $enc)
}

function Copy-Verified([string]$src, [string]$dst, [switch]$HardLink) {
    if (-not (Test-Path -LiteralPath $src -PathType Leaf)) {
        throw "Missing source file: $src"
    }
    $d = Split-Path -Parent $dst
    if (-not (Test-Path -LiteralPath $d)) {
        New-Item -ItemType Directory -Path $d | Out-Null
    }
    $linked = $false
    if ($HardLink -and $HardLinkPayload) {
        $srcRoot = [IO.Path]::GetPathRoot($src)
        $dstRoot = [IO.Path]::GetPathRoot($dst)
        if ($srcRoot -ieq $dstRoot) {
            New-Item -ItemType HardLink -Path $dst -Target $src | Out-Null
            $linked = $true
        }
    }
    if (-not $linked) {
        Copy-Item -LiteralPath $src -Destination $dst
    }
    $s = Get-Item -LiteralPath $src
    $t = Get-Item -LiteralPath $dst
    if ($s.Length -ne $t.Length) {
        throw "Size mismatch after copy: $src -> $dst ($($s.Length) vs $($t.Length))"
    }
    $sh = (Get-FileHash -LiteralPath $src -Algorithm SHA256).Hash
    $th = (Get-FileHash -LiteralPath $dst -Algorithm SHA256).Hash
    if ($sh -ne $th) {
        throw "SHA256 mismatch after copy: $src -> $dst"
    }
    [void]$script:copied.Add([pscustomobject]@{
        Source = $src
        Path = $dst
        Size = $t.Length
        SHA256 = $th
    })
}

function Copy-Template([string]$name, [string]$relative) {
    $src = Join-Path $templateDir $name
    $dst = Join-Path $dest $relative
    Copy-Verified $src $dst
}

# --- project metadata ---
$versionLine = Select-String -LiteralPath (Join-Path $repo "Cargo.toml") -Pattern '^\s*version\s*=\s*"([^"]+)"' | Select-Object -First 1
$version = if ($versionLine) { $versionLine.Matches[0].Groups[1].Value } else { "unknown" }
$commit = "unknown"
$gitDirty = $true
try {
    $commit = (& git -C $repo rev-parse HEAD).Trim()
    # untracked diagnostics in out/ do not make the source tree dirty
    $gitDirty = ((& git -C $repo status --porcelain --untracked-files=no) -join "`n").Trim().Length -gt 0
} catch {
    Write-Host "WARNING: git metadata unavailable: $_"
}

# --- build the GUI binary ---
$cargo = (Get-Command cargo -ErrorAction SilentlyContinue).Source
if (-not $cargo) {
    $cargo = Join-Path $env:USERPROFILE ".cargo\bin\cargo.exe"
}
if (-not (Test-Path -LiteralPath $cargo)) {
    throw "cargo not found (PATH or %USERPROFILE%\.cargo\bin\cargo.exe)"
}
Push-Location $repo
try {
    if ($Configuration -eq "release") {
        & $cargo build --release --bin PocketTTS-GUI-Rust
    } else {
        & $cargo build --bin PocketTTS-GUI-Rust
    }
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build failed with exit code $LASTEXITCODE"
    }
} finally {
    Pop-Location
}
$exeSrc = Join-Path $repo ("target\" + $Configuration + "\PocketTTS-GUI-Rust.exe")
if (-not (Test-Path -LiteralPath $exeSrc -PathType Leaf)) {
    throw "Built GUI exe not found: $exeSrc"
}

# --- dependency sources ---
$cpuRoot = Join-Path $repo "deps\ort-cpu\onnxruntime-win-x64-1.23.2"
$cpuLib = Join-Path $cpuRoot "lib"
$cudaSrc = Join-Path $repo "deps\portable"
$cpuVersion = (Get-Content -LiteralPath (Join-Path $cpuRoot "VERSION_NUMBER") -Raw).Trim()

# Microsoft Visual C++ runtime: the exe and ORT DLLs import these. They are
# redistributable, and bundling them next to the exe avoids requiring a
# separately installed VC++ Redistributable on a clean Windows machine.
$vcFiles = @("msvcp140.dll", "msvcp140_1.dll", "vcruntime140.dll", "vcruntime140_1.dll")
if ($VcRuntimeDir) {
    $vcDir = [IO.Path]::GetFullPath($VcRuntimeDir)
} else {
    $vcDir = Join-Path $env:WINDIR "System32"
}
foreach ($f in $vcFiles) {
    if (-not (Test-Path -LiteralPath (Join-Path $vcDir $f) -PathType Leaf)) {
        throw "Visual C++ runtime DLL missing: $($vcDir)\$f (use -VcRuntimeDir)"
    }
}

$requiredCuda = @(
    "onnxruntime.dll",
    "onnxruntime_providers_cuda.dll",
    "onnxruntime_providers_shared.dll",
    "cudart64_12.dll",
    "cublas64_12.dll",
    "cublasLt64_12.dll",
    "cudnn64_9.dll",
    "cudnn_adv64_9.dll",
    "cudnn_cnn64_9.dll",
    "cudnn_engines_precompiled64_9.dll",
    "cudnn_engines_runtime_compiled64_9.dll",
    "cudnn_graph64_9.dll",
    "cudnn_heuristic64_9.dll",
    "cudnn_ops64_9.dll",
    "cufft64_11.dll"
)

$modelFiles = @(
    "flow_lm_main.onnx",
    "flow_lm_flow.onnx",
    "mimi_decoder.onnx",
    "mimi_encoder.onnx",
    "text_conditioner.onnx",
    "tokenizer.model"
)

# G6.3: три квантованных графа на модель (main/flow/decoder); encoder/text
# INT8-версий не имеют и всегда берутся FP32. -SkipInt8Models — компактный
# комплект без режима INT8.
$int8ModelFiles = @(
    "flow_lm_main_int8.onnx",
    "flow_lm_flow_int8.onnx",
    "mimi_decoder_int8.onnx"
)

if (-not $SkipModels) {
    if ($ModelsDir) {
        $modelsRoot = [IO.Path]::GetFullPath($ModelsDir)
    } else {
        $modelsRoot = Join-Path $repo "deps\models"
        if (-not (Test-Path -LiteralPath $modelsRoot)) {
            throw "ModelsDir not specified and deps\models does not exist"
        }
    }
    foreach ($m in @("ru-v2-fast", "ru-v2-slow")) {
        $dir = Join-Path $modelsRoot $m
        if (-not (Test-Path -LiteralPath $dir)) {
            throw "Model directory missing: $dir"
        }
        foreach ($f in $modelFiles) {
            if (-not (Test-Path -LiteralPath (Join-Path $dir $f) -PathType Leaf)) {
                throw "Model file missing: $($dir)\$f"
            }
        }
        if (-not $SkipInt8Models) {
            foreach ($f in $int8ModelFiles) {
                if (-not (Test-Path -LiteralPath (Join-Path $dir $f) -PathType Leaf)) {
                    throw "INT8 model file missing: $($dir)\$f (use -SkipInt8Models for an FP32-only kit)"
                }
            }
        }
    }
}

if (-not $SkipFfmpeg) {
    if ($Ffmpeg) {
        $ffmpegSrc = [IO.Path]::GetFullPath($Ffmpeg)
    } else {
        $c1 = Join-Path $repo "deps\ffmpeg\ffmpeg.exe"
        $c2 = Join-Path $repo "ffmpeg\ffmpeg.exe"
        if (Test-Path -LiteralPath $c1) { $ffmpegSrc = $c1 }
        elseif (Test-Path -LiteralPath $c2) { $ffmpegSrc = $c2 }
        else { throw "Ffmpeg not specified and no deps\ffmpeg\ffmpeg.exe" }
    }
    if (-not (Test-Path -LiteralPath $ffmpegSrc -PathType Leaf)) {
        throw "ffmpeg.exe not found: $ffmpegSrc"
    }
}

if ($VoiceFile) {
    $voiceSrc = [IO.Path]::GetFullPath($VoiceFile)
    if (-not (Test-Path -LiteralPath $voiceSrc -PathType Leaf)) {
        throw "Voice file not found: $voiceSrc"
    }
    if (-not $voiceSrc.ToLowerInvariant().EndsWith(".wav")) {
        throw "Voice file must be WAV: $voiceSrc"
    }
}

# --- layout ---
foreach ($d in @(
    "onnxruntime\cpu",
    "onnxruntime\cuda",
    "ffmpeg",
    "models",
    "input",
    "output",
    "voice",
    "docs",
    "LICENSES"
)) {
    Ensure-Dir $d
}

# --- binaries ---
Copy-Verified $exeSrc (Join-Path $dest "PocketTTS-GUI-Rust.exe")
foreach ($f in $vcFiles) {
    Copy-Verified (Join-Path $vcDir $f) (Join-Path $dest $f)
}
Copy-Verified (Join-Path $cpuLib "onnxruntime.dll") (Join-Path $dest "onnxruntime\cpu\onnxruntime.dll") -HardLink
Copy-Verified (Join-Path $cpuLib "onnxruntime_providers_shared.dll") (Join-Path $dest "onnxruntime\cpu\onnxruntime_providers_shared.dll") -HardLink

if (-not $SkipCuda) {
    foreach ($name in $requiredCuda) {
        Copy-Verified (Join-Path $cudaSrc $name) (Join-Path $dest "onnxruntime\cuda\$name") -HardLink
    }
}

if (-not $SkipFfmpeg) {
    Copy-Verified $ffmpegSrc (Join-Path $dest "ffmpeg\ffmpeg.exe") -HardLink
}

if (-not $SkipModels) {
    foreach ($m in @("ru-v2-fast", "ru-v2-slow")) {
        Ensure-Dir ("models\" + $m)
        foreach ($f in $modelFiles) {
            Copy-Verified (Join-Path $modelsRoot "$m\$f") (Join-Path $dest "models\$m\$f") -HardLink
        }
        if (-not $SkipInt8Models) {
            foreach ($f in $int8ModelFiles) {
                Copy-Verified (Join-Path $modelsRoot "$m\$f") (Join-Path $dest "models\$m\$f") -HardLink
            }
        }
    }
} else {
    Write-Utf8NoBom (Join-Path $dest "models\README-MISSING.txt") @"
Models were intentionally skipped (-SkipModels).
Place FP32 ru-v2-fast and ru-v2-slow directories here (FP32 graphs are the
default). For the INT8 mode also add flow_lm_main_int8.onnx,
flow_lm_flow_int8.onnx and mimi_decoder_int8.onnx into each model directory.
"@
}

if ($VoiceFile) {
    Copy-Verified $voiceSrc (Join-Path $dest "voice\reference.wav")
} else {
    Copy-Template "VOICE-README.txt" "voice\README.txt"
}

# --- settings and examples ---
Copy-Template "settings.json" "PocketTTS-GUI-Rust.json"
Copy-Template "settings.json" "PocketTTS-GUI-Rust.example.json"
Copy-Template "example.txt" "input\example.txt"
Copy-Template "README.txt" "README-PORTABLE.txt"
Copy-Template "THIRD_PARTY_NOTICES.md" "THIRD_PARTY_NOTICES.md"
Copy-Template "NVIDIA-CUDA-NOTICE.txt" "LICENSES\NVIDIA-CUDA-NOTICE.txt"
Copy-Template "FFmpeg-NOTICE.txt" "LICENSES\FFmpeg-NOTICE.txt"
Copy-Template "MODEL-NOTICE.txt" "LICENSES\MODEL-NOTICE.txt"
Copy-Template "MSVC-RUNTIME-NOTICE.txt" "LICENSES\MSVC-RUNTIME-NOTICE.txt"

$doc = Join-Path $repo "docs\PORTABLE_BUILD.md"
if (-not (Test-Path -LiteralPath $doc -PathType Leaf)) {
    throw "Documentation missing: $doc"
}
Copy-Verified $doc (Join-Path $dest "docs\PORTABLE_BUILD.md")

# --- licenses ---
Copy-Verified (Join-Path $repo "LICENSE-TERATTS-ANDROID") (Join-Path $dest "LICENSES\GPL-3.0.txt")
Copy-Verified (Join-Path $cpuRoot "LICENSE") (Join-Path $dest "LICENSES\ONNX-Runtime-LICENSE.txt")
Copy-Verified (Join-Path $cpuRoot "ThirdPartyNotices.txt") (Join-Path $dest "LICENSES\ONNX-Runtime-ThirdPartyNotices.txt")
Copy-Verified (Join-Path $cpuRoot "VERSION_NUMBER") (Join-Path $dest "LICENSES\ONNX-Runtime-VERSION.txt")
Copy-Verified (Join-Path $repo "LICENSE-TERATTS-ANDROID") (Join-Path $dest "LICENSES\FFmpeg-GPLv3.txt")
Copy-Verified (Join-Path $repo "Cargo.lock") (Join-Path $dest "LICENSES\Rust-Cargo.lock.txt")

Write-Utf8NoBom (Join-Path $dest "output\README.txt") @"
Generated audio files are written here by default.
Existing files are never overwritten: rename the output or choose another directory.
"@

# --- required files check ---
$required = @(
    "PocketTTS-GUI-Rust.exe",
    "PocketTTS-GUI-Rust.json",
    "PocketTTS-GUI-Rust.example.json",
    "onnxruntime\cpu\onnxruntime.dll",
    "onnxruntime\cpu\onnxruntime_providers_shared.dll",
    "README-PORTABLE.txt",
    "THIRD_PARTY_NOTICES.md",
    "docs\PORTABLE_BUILD.md",
    "LICENSES\GPL-3.0.txt",
    "LICENSES\ONNX-Runtime-LICENSE.txt",
    "LICENSES\ONNX-Runtime-ThirdPartyNotices.txt",
    "LICENSES\FFmpeg-GPLv3.txt",
    "LICENSES\NVIDIA-CUDA-NOTICE.txt",
    "LICENSES\MODEL-NOTICE.txt",
    "LICENSES\MSVC-RUNTIME-NOTICE.txt",
    "msvcp140.dll",
    "msvcp140_1.dll",
    "vcruntime140.dll",
    "vcruntime140_1.dll"
)
if (-not $SkipCuda) {
    foreach ($name in $requiredCuda) {
        $required += "onnxruntime\cuda\$name"
    }
}
if (-not $SkipFfmpeg) {
    $required += "ffmpeg\ffmpeg.exe"
}
if (-not $SkipModels) {
    foreach ($m in @("ru-v2-fast", "ru-v2-slow")) {
        foreach ($f in $modelFiles) {
            $required += "models\$m\$f"
        }
    }
}
foreach ($rel in $required) {
    $p = Join-Path $dest $rel
    if (-not (Test-Path -LiteralPath $p -PathType Leaf)) {
        throw "Required file missing in distribution: $rel"
    }
}

# --- semantic checks ---
$exeInfo = Get-Item -LiteralPath (Join-Path $dest "PocketTTS-GUI-Rust.exe")
if ($exeInfo.Length -lt 1000000) {
    throw "PocketTTS-GUI-Rust.exe looks too small: $($exeInfo.Length) bytes"
}

if (-not $SkipModels) {
    foreach ($m in @("ru-v2-fast", "ru-v2-slow")) {
        $main = Get-Item -LiteralPath (Join-Path $dest "models\$m\flow_lm_main.onnx")
        if ($main.Length -lt 100000000) {
            throw "flow_lm_main.onnx is unexpectedly small (not FP32?): $($main.FullName)"
        }
        if (-not $SkipInt8Models) {
            $int8Main = Get-Item -LiteralPath (Join-Path $dest "models\$m\flow_lm_main_int8.onnx")
            if ($int8Main.Length -lt 50000000 -or $int8Main.Length -gt 100000000) {
                throw "flow_lm_main_int8.onnx has an unexpected size (not the quantized graph?): $($int8Main.FullName)"
            }
        }
    }
}

$ffmpegVersion = "not bundled"
if (-not $SkipFfmpeg) {
    $ffExe = Join-Path $dest "ffmpeg\ffmpeg.exe"
    $ffOut = & $ffExe -version 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw "Bundled ffmpeg.exe does not run: $ffExe"
    }
    $ffmpegVersion = ($ffOut | Select-Object -First 1).ToString()
}

$settingsText = Get-Content -LiteralPath (Join-Path $dest "PocketTTS-GUI-Rust.json") -Raw
$null = $settingsText | ConvertFrom-Json

# --- manifest ---
$manifestPath = Join-Path $dest "manifest.json"
$fileEntries = @()
Get-ChildItem -LiteralPath $dest -Recurse -File | Where-Object { $_.FullName -ne $manifestPath } | Sort-Object FullName | ForEach-Object {
    $rel = $_.FullName.Substring($dest.Length).TrimStart('\')
    $fileEntries += [ordered]@{
        path = $rel
        size = $_.Length
        sha256 = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash
    }
}

$modelEntries = @()
if (-not $SkipModels) {
    foreach ($m in @("ru-v2-fast", "ru-v2-slow")) {
        foreach ($f in $modelFiles) {
            $p = Join-Path $dest "models\$m\$f"
            $i = Get-Item -LiteralPath $p
            $modelEntries += [ordered]@{
                name = "$m/$f"
                size = $i.Length
                sha256 = (Get-FileHash -LiteralPath $p -Algorithm SHA256).Hash
            }
        }
    }
}

$ortCudaVersion = if (-not $SkipCuda) {
    (Get-Item -LiteralPath (Join-Path $dest "onnxruntime\cuda\onnxruntime.dll")).VersionInfo.FileVersion
} else {
    "not bundled"
}

$manifest = [ordered]@{
    product = "PocketTTS-GUI-Rust portable"
    version = $version
    git_commit = $commit
    git_dirty = $gitDirty
    built_at = (Get-Date).ToString("o")
    configuration = $Configuration
    onnxruntime = [ordered]@{
        cpu = $cpuVersion
        cuda = $ortCudaVersion
    }
    ffmpeg = $ffmpegVersion
    models = $modelEntries
    files = $fileEntries
}
Write-Utf8NoBom $manifestPath (($manifest | ConvertTo-Json -Depth 8) + "`n")

# --- summary ---
$allFiles = Get-ChildItem -LiteralPath $dest -Recurse -File
$totalBytes = ($allFiles | Measure-Object -Property Length -Sum).Sum
$exeHash = (Get-FileHash -LiteralPath (Join-Path $dest "PocketTTS-GUI-Rust.exe") -Algorithm SHA256).Hash

Write-Host ""
Write-Host "PORTABLE BUILD OK"
Write-Host "Destination : $dest"
Write-Host "Version     : $version (git $commit, dirty=$gitDirty)"
Write-Host "Files       : $($allFiles.Count)"
Write-Host "Total size  : $([math]::Round($totalBytes / 1MB, 1)) MiB"
Write-Host "Exe SHA256  : $exeHash"
Write-Host "Manifest    : $manifestPath"

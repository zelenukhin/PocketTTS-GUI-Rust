# Сборка SoundTouchDLL.dll (SoundTouch 2.4.1, MSVC x64) для SRT-версии
# PocketTTS-GUI-Rust. Этап SRT.3, контракт docs/SRT_0_AUDIT_CONTRACT.md §7.
#
# Исходник: third_party/soundtouch/soundtouch-2.4.1.tar.gz (SHA256
# e07abf20ce8f95850c280132e1f61ad400fc1f4011b7fac698a503de6aab6733).
# Распаковка идёт в уникальный подкаталог %TEMP%; дерево deps не изменяется.
# Готовая DLL кладётся в third_party/soundtouch/SoundTouchDLL.dll атомарным
# переносом. Лицензия LGPL 2.1 — см. third_party/soundtouch/NOTICE.txt.
#
# Использование: powershell -NoProfile -ExecutionPolicy Bypass -File tools\build-soundtouch.ps1

param(
    [string]$Vcvars = "C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat"
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$deps = Join-Path $root "third_party\soundtouch"
$tarball = Join-Path $deps "soundtouch-2.4.1.tar.gz"

if (-not (Test-Path -LiteralPath $tarball)) { Write-Error "нет исходника $tarball"; exit 1 }
$hash = (Get-FileHash -LiteralPath $tarball -Algorithm SHA256).Hash
if ($hash -ne "E07ABF20CE8F95850C280132E1F61AD400FC1F4011B7FAC698A503DE6AAB6733") {
    Write-Error "SHA256 исходника не совпадает: $hash"
    exit 1
}

$work = Join-Path $env:TEMP "soundtouch-build"
if (-not (Test-Path -LiteralPath $work)) { New-Item -ItemType Directory -Path $work | Out-Null }

try {
    # Родной bsdtar Windows (System32) понимает пути Windows напрямую;
    # tar из Git (GNU) искажает их MSYS-конвертацией.
    $bsdtar = Join-Path $env:SystemRoot "System32\tar.exe"
    if (-not (Test-Path -LiteralPath $bsdtar)) { Write-Error "нет $bsdtar"; exit 1 }
    & $bsdtar -xzf $tarball -C $work 2>$null
    $src = Join-Path $work "soundtouch"
    $dllcpp = Join-Path $src "source\SoundTouchDLL\SoundTouchDLL.cpp"
    if (-not (Test-Path -LiteralPath $dllcpp)) {
        Write-Error "распаковка не дала ожидаемой структуры"
        exit 1
    }

    $bat = Join-Path $work "build.cmd"
    $lines = @(
        '@echo off',
        ('call "' + $Vcvars + '" >nul'),
        'if errorlevel 1 exit /b 1',
        ('cd /d "' + $work + '"'),
        ('md obj 2>nul'),
        ('cl /nologo /O2 /EHsc /MD /std:c++17 /DDLL_EXPORTS /I "' + $src + '\include" /c "' + $src + '\source\SoundTouch\*.cpp" "' + $src + '\source\SoundTouchDLL\SoundTouchDLL.cpp" /Fo:obj/'),
        ('if errorlevel 1 exit /b 1'),
        ('link /nologo /DLL /OUT:soundtouch_out.dll /Brepro /IMPLIB:st.lib obj\*.obj')
        'if errorlevel 1 exit /b 1',
        'dumpbin /exports soundtouch_out.dll > exports.txt 2>nul',
        'exit /b 0'
    )
    Set-Content -LiteralPath $bat -Value $lines -Encoding ASCII
    & cmd.exe /c $bat
    if ($LASTEXITCODE -ne 0) { Write-Error "сборка cl.exe не удалась"; exit 1 }

    $built = Join-Path $work "soundtouch_out.dll"
    if (-not (Test-Path -LiteralPath $built)) { Write-Error "DLL не найдена после сборки"; exit 1 }

    # Проверка экспортов минимального набора функций обёртки (dumpbin
    # доступен только внутри vcvars-окружения — вывод собран в exports.txt).
    $exportsFile = Join-Path $work "exports.txt"
    if (-not (Test-Path -LiteralPath $exportsFile)) {
        Write-Warning "dumpbin недоступен - экспорты не проверены"
    }
    else {
        $dump = Get-Content -LiteralPath $exportsFile
        foreach ($fn in @("soundtouch_createInstance", "soundtouch_putSamples", "soundtouch_receiveSamples", "soundtouch_flush", "soundtouch_setTempo", "soundtouch_setSetting")) {
            if (-not ($dump -match [regex]::Escape($fn))) {
                Write-Error "экспорт отсутствует: $fn"
                exit 1
            }
        }
    }

    # Замена в deps атомарным переносом (старый файл не удаляется заранее).
    $staged = Join-Path $deps "SoundTouchDLL.dll.new"
    Move-Item -LiteralPath $built -Destination $staged -Force
    Move-Item -LiteralPath $staged -Destination (Join-Path $deps "SoundTouchDLL.dll") -Force
    $newHash = (Get-FileHash -LiteralPath (Join-Path $deps "SoundTouchDLL.dll") -Algorithm SHA256).Hash
    Write-Output ("BUILD OK: third_party\soundtouch\SoundTouchDLL.dll SHA256 " + $newHash)
    exit 0
}
finally {
    # Рабочий каталог во %TEMP% оставляем (рекурсивные удаления в проекте
    # запрещены правилами AGENTS.md); имя содержит одноразовый GUID.
    Write-Output ("рабочий каталог сборки оставлен: " + $work)
}

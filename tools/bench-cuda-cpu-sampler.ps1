# CUDA-P2B.1: сэмплер загрузки CPU для диагностических прогонов.
# Пишет ts_ms,cpu_pct каждые ~500 мс; остановка — появлением StopFile.
# Только собственный процесс; чужие процессы не трогает.
param(
    [Parameter(Mandatory)][string]$OutCsv,
    [Parameter(Mandatory)][string]$StopFile
)
$ErrorActionPreference = 'Stop'
if (Test-Path -LiteralPath $OutCsv) { throw "CSV уже существует: $OutCsv" }
$fs = [System.IO.File]::Open($OutCsv, 'CreateNew')
$w = New-Object System.IO.StreamWriter($fs)
try {
    $w.WriteLine('ts_ms,cpu_pct')
    $w.Flush()
    while ($true) {
        if (Test-Path -LiteralPath $StopFile) { break }
        $ts = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
        $cpu = $null
        try {
            $cpu = (Get-CimInstance Win32_PerfFormattedData_PerfOS_Processor -Filter "Name='_Total'" -Property PercentProcessorTime).PercentProcessorTime
        } catch { }
        if ($null -ne $cpu) {
            $w.WriteLine("$ts,$cpu")
            $w.Flush()
        }
        Start-Sleep -Milliseconds 500
    }
} finally {
    $w.Dispose(); $fs.Dispose()
}

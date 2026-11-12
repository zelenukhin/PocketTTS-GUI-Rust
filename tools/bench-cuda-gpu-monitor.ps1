# CUDA-P2B.1: монитор GPU/PCIe для диагностических прогонов.
# Два собственных дочерних nvidia-smi (loop-query + dmon PCIe); по StopFile
# оба завершаются, CSV дописывается без потери буфера (чтение через pipe).
# Чужие процессы не трогает.
param(
    [Parameter(Mandatory)][string]$OutGpuCsv,
    [Parameter(Mandatory)][string]$OutPcieCsv,
    [Parameter(Mandatory)][string]$StopFile
)
$ErrorActionPreference = 'Stop'
foreach ($f in @($OutGpuCsv, $OutPcieCsv)) {
    if (Test-Path -LiteralPath $f) { throw "CSV уже существует: $f" }
}
$gpuFs = [System.IO.File]::Open($OutGpuCsv, 'CreateNew')
$gpuW = New-Object System.IO.StreamWriter($gpuFs)
$pcieFs = [System.IO.File]::Open($OutPcieCsv, 'CreateNew')
$pcieW = New-Object System.IO.StreamWriter($pcieFs)
$gpuW.WriteLine('ts_ms,util_gpu,util_mem,vram_mb,power_w,clocks_sm,pstate')
$pcieW.WriteLine('ts_ms,rx_mb_s,tx_mb_s')
$gpuW.Flush(); $pcieW.Flush()

$nvsmi = (Get-Command nvidia-smi.exe).Source
$gsi = New-Object System.Diagnostics.ProcessStartInfo
$gsi.FileName = $nvsmi
$gsi.Arguments = '--query-gpu=utilization.gpu,utilization.memory,memory.used,power.draw,clocks.sm,pstate --format=csv,noheader,nounits --loop-ms=100'
$gsi.RedirectStandardOutput = $true
$gsi.UseShellExecute = $false
$gsi.CreateNoWindow = $true
$gp = [System.Diagnostics.Process]::Start($gsi)

$dsi = New-Object System.Diagnostics.ProcessStartInfo
$dsi.FileName = $nvsmi
$dsi.Arguments = 'dmon -s t -d 1 -c 3600'
$dsi.RedirectStandardOutput = $true
$dsi.UseShellExecute = $false
$dsi.CreateNoWindow = $true
$dp = [System.Diagnostics.Process]::Start($dsi)

$gt = $gp.StandardOutput.ReadLineAsync()
$pt = $dp.StandardOutput.ReadLineAsync()
try {
    while ($true) {
        if (Test-Path -LiteralPath $StopFile) { break }
        $tasks = @()
        if ($gt) { $tasks += $gt }
        if ($pt) { $tasks += $pt }
        if ($tasks.Count -eq 0) { break }
        $idx = [System.Threading.Tasks.Task]::WaitAny([System.Threading.Tasks.Task[]]$tasks, 250)
        if ($idx -lt 0) { continue }
        if ($gt -and $gt.IsCompleted) {
            $line = $null
            try { $line = $gt.Result } catch { $line = $null }
            $ts = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
            if ($null -eq $line) {
                $gt = $null
            } else {
                $p = ($line -split ',') | ForEach-Object { $_.Trim() }
                if ($p.Count -ge 6 -and $p[0] -match '^\d+$') {
                    $gpuW.WriteLine(($ts.ToString() + ',' + ($p -join ',')))
                    $gpuW.Flush()
                }
                $gt = $gp.StandardOutput.ReadLineAsync()
            }
        }
        if ($pt -and $pt.IsCompleted) {
            $line = $null
            try { $line = $pt.Result } catch { $line = $null }
            $ts = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
            if ($null -eq $line) {
                $pt = $null
            } else {
                if (-not $line.StartsWith('#')) {
                    $q = ($line -split '\s+') | Where-Object { $_ -ne '' }
                    if ($q.Count -ge 3) {
                        $pcieW.WriteLine(($ts.ToString() + ',' + $q[-2] + ',' + $q[-1]))
                        $pcieW.Flush()
                    }
                }
                $pt = $dp.StandardOutput.ReadLineAsync()
            }
        }
    }
} finally {
    foreach ($p in @($gp, $dp)) {
        if ($null -ne $p -and -not $p.HasExited) {
            try { $p.Kill() } catch { }
            try { $p.WaitForExit(3000) | Out-Null } catch { }
        }
    }
    $gpuW.Dispose(); $gpuFs.Dispose()
    $pcieW.Dispose(); $pcieFs.Dispose()
}

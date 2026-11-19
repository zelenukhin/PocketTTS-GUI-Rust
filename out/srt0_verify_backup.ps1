# Верификация NAS-бэкапа SRT.0: SHA256 удалённого + локальной копии (байт-в-байт)
# + читаемость всех entry архива (CRC) из локальной копии.
param([Parameter(Mandatory=$true)][string]$Path)

$remote = $null
foreach ($p in @($Path, ($Path -replace '/', '\')) ) { try { $remote = (Resolve-Path -LiteralPath $p).Path; break } catch {} }
if (-not $remote) { Write-Output "FAILED: path not found: $Path"; exit 1 }

$hRemote = (Get-FileHash -LiteralPath $remote -Algorithm SHA256).Hash

$tmp = Join-Path $env:TEMP ("srt0_verify_" + [guid]::NewGuid().ToString('N') + ".zip")
Copy-Item -LiteralPath $remote -Destination $tmp
$hLocal = (Get-FileHash -LiteralPath $tmp -Algorithm SHA256).Hash

Write-Output "SHA256 remote: $hRemote"
Write-Output "SHA256 local : $hLocal"
if ($hRemote -ne $hLocal) { Remove-Item -LiteralPath $tmp; Write-Output "FAILED: hash mismatch"; exit 1 }

Add-Type -AssemblyName System.IO.Compression.FileSystem
$z = [System.IO.Compression.ZipFile]::OpenRead($tmp)
$bad = 0
foreach ($e in $z.Entries) {
    try {
        $s = $e.Open()
        $buf = New-Object byte[] 65536
        while ($s.Read($buf, 0, 65536) -gt 0) {}
        $s.Close()
    } catch { $bad++ }
}
Write-Output ("entries: {0} unreadable: {1}" -f $z.Entries.Count, $bad)
$z.Dispose()
Remove-Item -LiteralPath $tmp
if ($bad -gt 0) { Write-Output "FAILED: unreadable entries"; exit 1 }
Write-Output "VERIFY OK"

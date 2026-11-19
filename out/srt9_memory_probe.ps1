# SRT.9D memory probe: run real CPU SRT tests while sampling private bytes
# of all test processes under target\. ASCII-only output.
$ErrorActionPreference = "Stop"
$repo = "C:\PocketTTS-GUI-Rust\ShtormTTS-Windows"
$env:SRT_TEST_ONNX_BASE = "C:\Users\alexa\Downloads\pocket_tts_android\build\onnx"
$env:Path = "$env:USERPROFILE\.cargo\bin;" + $env:Path

$p = Start-Process -FilePath "cargo" `
    -ArgumentList "test -p pockettts-gui-rust --release --lib srt_job::tests::real_ -- --ignored --test-threads=1" `
    -WorkingDirectory $repo -NoNewWindow -PassThru `
    -RedirectStandardOutput "$env:TEMP\srt9_mem_out.txt" -RedirectStandardError "$env:TEMP\srt9_mem_err.txt"

$peak = 0L
$peakName = ""
$samples = 0
while (-not $p.HasExited) {
    $targets = Get-Process | Where-Object { $_.Path -like "$repo\target\*" }
    foreach ($t in $targets) {
        $pm = $t.PrivateMemorySize64
        if ($pm -gt $peak) { $peak = $pm; $peakName = $t.ProcessName }
    }
    $samples++
    Start-Sleep -Milliseconds 300
}
$p.WaitForExit()
$out = Get-Content "$env:TEMP\srt9_mem_out.txt" -Raw
Write-Output ("RESULT: " + (($out -split "`n" | Where-Object { $_ -match "test result" }) -join " | "))
Write-Output "SAMPLES: $samples  PEAK PRIVATE BYTES: $peak ($([math]::Round($peak/1MB,1)) MB) on '$peakName'"
if (-not $out.Contains("test result: ok. 3 passed; 0 failed")) { throw "tests failed" }
Write-Output "SRT9 MEMORY PROBE PASS"

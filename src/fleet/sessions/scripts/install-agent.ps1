$ErrorActionPreference = 'Stop'
$d = Join-Path $env:USERPROFILE '.muxen\bin'
$t = Join-Path $d 'muxen-agent.exe'
$n = Join-Path $d 'muxen-agent.upload.exe'
$p = Join-Path $d "muxen-agent.old.$PID.$([Guid]::NewGuid().ToString('N')).exe"
$expected = '__EXPECTED_HASH__'
$swapped = $false
$previous = $false
$restarted = $false
function Check-Hash($path) {
    $hash = (& $path hash | Out-String).Trim()
    if ($LASTEXITCODE -ne 0 -or $hash -ne $expected) { throw 'The agent failed its hash check.' }
}
function Restart-Agent {
    Stop-ScheduledTask -TaskName 'Muxen agent' -ErrorAction Stop
    Start-ScheduledTask -TaskName 'Muxen agent' -ErrorAction Stop
}
try {
    Check-Hash $n
    if (Test-Path $t) {
        # ReplaceFile keeps the backup and replaces the live path atomically.
        [IO.File]::Replace($n, $t, $p)
        $previous = $true
    } else {
        [IO.File]::Move($n, $t)
    }
    $swapped = $true
    Check-Hash $t
    $task = Get-ScheduledTask | Where-Object { $_.TaskName -eq 'Muxen agent' -and $_.TaskPath -eq '\' }
    if ($task) {
        $restarted = $true
        Restart-Agent
    }
    Get-CimInstance Win32_Process -Filter "Name='muxen-agent.exe'" | Where-Object { $_.ExecutablePath -like "$d\*" -and $_.CommandLine -match ' gui-serve' } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction Stop }
    $swapped = $false
    $expected
} catch {
    $problem = $_.Exception.Message
    if ($swapped) {
        try {
            if ($previous) {
                $restore = Join-Path $d "muxen-agent.restore.$PID.exe"
                Copy-Item -LiteralPath $p -Destination $restore -Force
                [IO.File]::Replace($restore, $t, $null)
                $problem += ' Restored the previous agent.'
            } else {
                Remove-Item -LiteralPath $t -Force
            }
            if ($restarted) { Restart-Agent }
        } catch {
            $problem += " Could not finish restoring the agent: $($_.Exception.Message)"
        }
    }
    throw $problem
} finally {
    if (Test-Path $n) { Remove-Item -LiteralPath $n -Force }
    # Older images may still serve sessions. A locked backup can wait until
    # the next update; it must never block creating this update's backup.
    Get-ChildItem -LiteralPath $d -Filter 'muxen-agent.old.*.exe' -ErrorAction SilentlyContinue |
        Where-Object { $_.FullName -ne $p } |
        ForEach-Object { Remove-Item -LiteralPath $_.FullName -Force -ErrorAction SilentlyContinue }
}

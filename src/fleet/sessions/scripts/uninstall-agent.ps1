$ErrorActionPreference = 'Stop'
function Removed($item) { "removed`t$item" }
function Failed($item) { "failed`t$item" }
$d = Join-Path $env:USERPROFILE '.muxen'
try {
    $tasks = Get-ScheduledTask | Where-Object {
        ($_.TaskPath -eq '\' -and $_.TaskName -eq 'Muxen agent') -or
        ($_.TaskPath -eq '\Muxen\' -and $_.TaskName -like 'Muxen schedule s-*')
    }
    foreach ($task in $tasks) {
        try {
            $task | Stop-ScheduledTask
            $task | Unregister-ScheduledTask -Confirm:$false
            Removed "Task $($task.TaskPath)$($task.TaskName)"
        } catch { Failed "Task $($task.TaskName): $($_.Exception.Message)" }
    }
} catch { Failed "Could not read the agent tasks: $($_.Exception.Message)" }
try {
    Get-NetFirewallRule -DisplayName 'Muxen agent' -ErrorAction Stop | Remove-NetFirewallRule
    Removed 'Muxen agent firewall rule'
} catch {
    if ($_.CategoryInfo.Category -ne 'ObjectNotFound') { Failed "Firewall rule: $($_.Exception.Message)" }
}
try {
    Get-CimInstance Win32_Process | Where-Object { $_.ExecutablePath -like "$d\bin\muxen-agent*" } | ForEach-Object {
        try {
            Stop-Process -Id $_.ProcessId -Force
            Removed "Agent process $($_.ProcessId)"
        } catch { Failed "Agent process $($_.ProcessId): $($_.Exception.Message)" }
    }
} catch { Failed "Could not read the agent processes: $($_.Exception.Message)" }
try {
    if (Test-Path -LiteralPath $d) {
        Remove-Item -LiteralPath $d -Recurse -Force
        Removed $d
    }
} catch { Failed "${d}: $($_.Exception.Message)" }
'complete'

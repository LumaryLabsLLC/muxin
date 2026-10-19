# Muxin: control one installed runner (Windows).
# Muxin prepends: $OR_ACTION (start|stop|restart|logs|remove), $OR_DIR,
# $OR_TOKEN (remove only), $OR_DELETE (bool).

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
trap { Write-Error ("error: " + $_.Exception.Message); exit 1 }
function say($m) { Write-Output $m }

if (-not $OR_DIR -or -not (Test-Path $OR_DIR)) { throw "$OR_DIR doesn't exist." }
Set-Location $OR_DIR

$svcFile = Join-Path $OR_DIR '.service'
$svcName = if (Test-Path $svcFile) { (Get-Content $svcFile | Select-Object -First 1).Trim() } else { $null }

function Get-RunnerProcesses {
  Get-CimInstance Win32_Process -Filter "Name='Runner.Listener.exe' OR Name='Runner.Worker.exe'" |
    Where-Object { $_.ExecutablePath -and $_.ExecutablePath.StartsWith("$OR_DIR\bin\", [StringComparison]::OrdinalIgnoreCase) }
}

# Starts a program that outlives this session. Muxin runs this script over ssh,
# and sshd ends everything the connection that started it leaves behind when it
# closes, so a plain Start-Process leaves nothing running. A process made by WMI
# isn't in that session; the agent starts itself the same way (agent/src/sys.rs).
function Start-Detached($file, $dir) {
  $startup = New-CimInstance -ClassName Win32_ProcessStartup -ClientOnly -Property @{ ShowWindow = [uint16]0 }
  $cmd = 'cmd.exe /c ""' + $file + '""'
  $r = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{
    CommandLine = $cmd
    CurrentDirectory = $dir
    ProcessStartupInformation = $startup
  }
  if ($r.ReturnValue -ne 0) { throw "Couldn't start $file (WMI returned $($r.ReturnValue))." }
}

function Start-Runner {
  if ($svcName) {
    say "Starting service $svcName…"
    Start-Service -Name $svcName
  } elseif (Get-RunnerProcesses) {
    say 'Already running.'
  } else {
    say 'Starting run.cmd in the background (no service installed)…'
    Start-Detached (Join-Path $OR_DIR 'run.cmd') $OR_DIR
    # It takes a moment to come up. Say it started only if it really did.
    for ($i = 0; $i -lt 10 -and -not (Get-RunnerProcesses); $i++) { Start-Sleep -Milliseconds 500 }
    if (-not (Get-RunnerProcesses)) { throw 'run.cmd started, but no runner process is running.' }
  }
}

function Stop-Runner {
  if ($svcName) {
    say "Stopping service $svcName…"
    Stop-Service -Name $svcName -Force
    return
  }
  $procs = @(Get-RunnerProcesses)
  if ($procs.Count -eq 0) { say 'Not running.'; return }
  foreach ($p in $procs) { Stop-Process -Id $p.ProcessId -Force -ErrorAction SilentlyContinue }
  say 'Stopped.'
}

switch ($OR_ACTION) {
  'start' { Start-Runner }
  'stop' { Stop-Runner }
  'restart' { Stop-Runner; Start-Sleep -Seconds 1; Start-Runner }
  'logs' {
    $log = Get-ChildItem -Path (Join-Path $OR_DIR '_diag') -Filter 'Runner_*.log' -ErrorAction SilentlyContinue |
      Sort-Object LastWriteTime -Descending | Select-Object -First 1
    if (-not $log) { say "No runner logs yet in $OR_DIR\_diag." }
    else { say "==> $($log.FullName) (last 200 lines)"; Get-Content -Path $log.FullName -Tail 200 }
  }
  'remove' {
    Stop-Runner
    if (Test-Path (Join-Path $OR_DIR '.runner')) {
      say 'Removing the runner from GitHub…'
      # config.cmd remove also uninstalls the Windows service. The token goes in
      # its environment, not on its command line, which other accounts can read.
      $env:ACTIONS_RUNNER_INPUT_TOKEN = $OR_TOKEN
      try { & .\config.cmd remove } finally { Remove-Item Env:\ACTIONS_RUNNER_INPUT_TOKEN -ErrorAction SilentlyContinue }
      if ($LASTEXITCODE -ne 0) { throw "config.cmd remove failed (exit $LASTEXITCODE)" }
    }
    if ($OR_DELETE) {
      if ((Test-Path (Join-Path $OR_DIR 'config.cmd')) -and (Test-Path (Join-Path $OR_DIR 'bin'))) {
        Set-Location $env:SystemDrive\
        Remove-Item -Recurse -Force $OR_DIR
        say "Deleted $OR_DIR"
      } else {
        throw "$OR_DIR doesn't look like a runner folder; not deleting it."
      }
    }
    say '✓ Removed'
  }
  default { throw "Unknown action: $OR_ACTION" }
}

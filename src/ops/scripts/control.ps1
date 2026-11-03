# Muxin: control one installed runner (Windows).
# Muxin prepends: $OR_ACTION (start|stop|restart|logs|remove|update), $OR_DIR,
# $OR_TOKEN (remove only), $OR_DELETE (bool), and for update $OR_VERSION and
# $OR_SHA256 (array of "<platform>=<hex>": each package's SHA-256).

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
# A plain line, not an error record: PowerShell writes those as CLIXML when
# stderr isn't a terminal.
trap { [Console]::Error.WriteLine("error: " + $_.Exception.Message); exit 1 }
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
  $r.ProcessId
}

function Start-Runner {
  if ($svcName) {
    say "Starting service ${svcName}…"
    Start-Service -Name $svcName
  } elseif (Get-RunnerProcesses) {
    say 'Already running.'
  } else {
    say 'Starting run.cmd in the background (no service installed)…'
    $id = Start-Detached (Join-Path $OR_DIR 'run.cmd') $OR_DIR
    # It takes a moment to come up, longer on a slow machine. run.cmd runs as
    # long as the runner does, so it ending first means the runner didn't start.
    for ($i = 0; $i -lt 20 -and -not (Get-RunnerProcesses); $i++) {
      if (-not (Get-Process -Id $id -ErrorAction SilentlyContinue)) {
        throw "run.cmd stopped before the runner started. To see why, run $OR_DIR\run.cmd on the machine."
      }
      Start-Sleep -Milliseconds 500
    }
    if (-not (Get-RunnerProcesses)) { say 'Still starting; the check after this shows whether it came up.' }
  }
}

# Asks the listener to stop the way the runner's own service does: Ctrl+C in its
# console. It then deletes its session on GitHub and exits. Killed outright, it
# leaves that session behind for a few minutes, and a runner started in that time
# can't connect ("A session for this runner already exists"). Sending it takes a
# process of its own, one that can leave this console for the listener's and
# ignore the Ctrl+C it sends there. Says whether it was sent.
function Send-CtrlC($id) {
  # Its progress (Add-Type loading a module) would reach this script's output as CLIXML.
  $helper = @'
$ProgressPreference = 'SilentlyContinue'
$ErrorActionPreference = 'Stop'
try {
  Add-Type -Namespace Muxin -Name Console -MemberDefinition '
    [DllImport("kernel32.dll")] public static extern bool FreeConsole();
    [DllImport("kernel32.dll")] public static extern bool AttachConsole(uint id);
    [DllImport("kernel32.dll")] public static extern bool SetConsoleCtrlHandler(IntPtr handler, bool add);
    [DllImport("kernel32.dll")] public static extern bool GenerateConsoleCtrlEvent(uint ctrlEvent, uint group);'
  [void][Muxin.Console]::FreeConsole()
  if (-not [Muxin.Console]::AttachConsole($id)) { exit 2 }
  [void][Muxin.Console]::SetConsoleCtrlHandler([IntPtr]::Zero, $true)
  if (-not [Muxin.Console]::GenerateConsoleCtrlEvent(0, 0)) { exit 3 }
  exit 0
} catch { exit 1 }
'@
  $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes("`$id = $id`n$helper"))
  # Whatever goes wrong there ends in the forced stop, not in this script failing.
  $ErrorActionPreference = 'Continue'
  & powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -EncodedCommand $encoded *> $null
  $LASTEXITCODE -eq 0
}

function Stop-Runner {
  if ($svcName) {
    say "Stopping service ${svcName}…"
    Stop-Service -Name $svcName -Force
    return
  }
  $procs = @(Get-RunnerProcesses)
  if ($procs.Count -eq 0) { say 'Not running.'; return }
  $listener = $procs | Where-Object { $_.Name -eq 'Runner.Listener.exe' } | Select-Object -First 1
  $sent = $false
  if ($listener) {
    # The cmd.exe running run.cmd would get the Ctrl+C too, and then wait on
    # "Terminate batch job (Y/N)?" in a window no one sees. The listener outlives it.
    $cmd = Get-CimInstance Win32_Process -Filter "ProcessId=$($listener.ParentProcessId)"
    if ($cmd -and $cmd.Name -eq 'cmd.exe' -and $cmd.CommandLine -match '/c .*run\.cmd') {
      Stop-Process -Id $cmd.ProcessId -Force -ErrorAction SilentlyContinue
    }
    $sent = Send-CtrlC $listener.ProcessId
  }
  # 30 seconds, as the service gives it: a running job is cancelled first.
  if ($sent) { Wait-Process -Id $listener.ProcessId -Timeout 30 -ErrorAction SilentlyContinue }
  $left = @(Get-RunnerProcesses)
  if ($left.Count -gt 0) {
    if ($sent) { say 'Still running after 30 seconds; ending it.' }
    elseif ($listener) { say "Couldn't send it Ctrl+C; ending it." }
    foreach ($p in $left) { Stop-Process -Id $p.ProcessId -Force -ErrorAction SilentlyContinue }
  }
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
  'update' {
    if (-not $OR_VERSION) { throw 'No version to update to.' }
    if (-not (Test-Path (Join-Path $OR_DIR 'config.cmd')) -or -not (Test-Path (Join-Path $OR_DIR 'bin'))) { throw "$OR_DIR doesn't look like a runner folder." }
    if (Get-RunnerProcesses | Where-Object { $_.Name -eq 'Runner.Worker.exe' }) { throw "It's running a job. Update it when it's idle." }
    $arch = switch ($env:PROCESSOR_ARCHITECTURE) {
      'AMD64' { 'x64' }
      'ARM64' { 'arm64' }
      default { throw "Unsupported CPU architecture: $env:PROCESSOR_ARCHITECTURE" }
    }
    $pkg = "actions-runner-win-$arch-$OR_VERSION.zip"
    $url = "https://github.com/actions/runner/releases/download/v$OR_VERSION/$pkg"
    $sha = $null
    foreach ($entry in $OR_SHA256) {
      if ($entry.StartsWith("win-$arch=", [StringComparison]::Ordinal)) { $sha = $entry.Substring("win-$arch=".Length) }
    }
    if (-not $sha) { throw "GitHub's release notes for actions-runner $OR_VERSION don't list a SHA-256 for $pkg, so it can't be checked. Not updating." }
    # Everything that can fail by itself comes first, with the runner still running.
    $new = Join-Path $OR_DIR '.muxin-update'
    $unpacked = Join-Path $new 'unpacked'
    if (Test-Path $new) { Remove-Item -LiteralPath $new -Recurse -Force }
    New-Item -ItemType Directory -Path $unpacked -Force | Out-Null
    $zip = Join-Path $new $pkg
    say "Downloading actions-runner $OR_VERSION for win-$arch…"
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing
    $got = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($got -ne $sha) {
      Remove-Item -LiteralPath $new -Recurse -Force
      throw "The downloaded $pkg doesn't match the SHA-256 GitHub published for it (expected $sha, got $got), so it was deleted. Nothing changed."
    }
    say "Checked $pkg against GitHub's SHA-256"
    Expand-Archive -Path $zip -DestinationPath $unpacked -Force
    if (-not (Test-Path (Join-Path $unpacked 'bin\Runner.Listener.exe'))) {
      Remove-Item -LiteralPath $new -Recurse -Force
      throw "$pkg has no runner in it. Nothing changed."
    }
    $wasRunning = [bool](Get-RunnerProcesses)
    if ($wasRunning) { Stop-Runner; Start-Sleep -Seconds 1 }
    say "Replacing the runner's programs (its registration and work folder stay)…"
    # bin.* and externals.*: left by the runner's own updates.
    Get-ChildItem -LiteralPath $OR_DIR -Directory -Force | Where-Object { $_.Name -in 'bin', 'externals' -or $_.Name -like 'bin.*' -or $_.Name -like 'externals.*' } | Remove-Item -Recurse -Force
    Copy-Item -Path (Join-Path $unpacked '*') -Destination $OR_DIR -Recurse -Force
    Remove-Item -LiteralPath $new -Recurse -Force
    if ($wasRunning) { Start-Runner }
    say "✓ Updated to $OR_VERSION"
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

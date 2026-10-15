# Muxin machine probe (Windows). Read-only.
# Prints key=value lines; runner N is reported as rN.<key>=<value>.
# $OR_DIRS (array of glob patterns) is prepended by Muxin.

$ErrorActionPreference = 'SilentlyContinue'
$ProgressPreference = 'SilentlyContinue'
function kv($k, $v) { Write-Output ("{0}={1}" -f $k, $v) }

$os = Get-CimInstance Win32_OperatingSystem
kv 'probe' 1
kv 'os' 'windows'
kv 'arch' $env:PROCESSOR_ARCHITECTURE
kv 'hostname' $env:COMPUTERNAME
kv 'os_version' $os.Caption
kv 'cores' ([Environment]::ProcessorCount)
$cpu = (Get-CimInstance Win32_Processor | Measure-Object -Property LoadPercentage -Average).Average
if ($cpu -ne $null) { kv 'cpu_pct' ([math]::Round($cpu, 1)) }
if ($os) {
  kv 'mem_total' ([int64]$os.TotalVisibleMemorySize * 1024)
  kv 'mem_used' (([int64]$os.TotalVisibleMemorySize - [int64]$os.FreePhysicalMemory) * 1024)
  kv 'uptime_s' ([int64]((Get-Date) - $os.LastBootUpTime).TotalSeconds)
}
$drive = Get-CimInstance Win32_LogicalDisk -Filter ("DeviceID='{0}'" -f $env:SystemDrive)
if ($drive) {
  kv 'disk_total' ([int64]$drive.Size)
  kv 'disk_free' ([int64]$drive.FreeSpace)
  kv 'disk_path' $env:SystemDrive
}

# --- Runners -----------------------------------------------------------------
$procs = @(Get-CimInstance Win32_Process -Filter "Name='Runner.Listener.exe' OR Name='Runner.Worker.exe'")
$dirs = New-Object System.Collections.Generic.List[string]
foreach ($p in $procs) {
  if ($p.ExecutablePath) { $dirs.Add((Split-Path (Split-Path $p.ExecutablePath -Parent) -Parent)) }
}
$patterns = @($OR_DIRS) + @(
  "$env:SystemDrive\actions-runner\*", "$env:SystemDrive\actions-runner*", "$env:SystemDrive\runners\*", "$env:SystemDrive\actions-runners\*",
  "$env:USERPROFILE\actions-runner*", "$env:USERPROFILE\runners\*"
)
foreach ($pattern in $patterns) {
  if (-not $pattern) { continue }
  $pattern = $pattern -replace '^~', $env:USERPROFILE
  foreach ($d in (Get-Item -Path $pattern -Force)) {
    if ($d.PSIsContainer -and (Test-Path (Join-Path $d.FullName '.runner'))) { $dirs.Add($d.FullName) }
  }
}

$i = 0
foreach ($d in ($dirs | Sort-Object -Unique)) {
  $cfg = Join-Path $d '.runner'
  if (-not (Test-Path $cfg)) { continue }
  $p = "r$i"
  kv "$p.dir" $d
  kv "$p.config" ((Get-Content -Raw $cfg) -replace "[\r\n]", '')
  $listener = $procs | Where-Object { $_.Name -eq 'Runner.Listener.exe' -and $_.ExecutablePath -like "$d\bin\*" } | Select-Object -First 1
  $worker = $procs | Where-Object { $_.Name -eq 'Runner.Worker.exe' -and $_.ExecutablePath -like "$d\bin\*" } | Select-Object -First 1
  kv "$p.listener" $listener.ProcessId
  kv "$p.worker" $worker.ProcessId
  $exe = Join-Path $d 'bin\Runner.Listener.exe'
  if (Test-Path $exe) { kv "$p.version" ((& $exe --version) | Select-Object -First 1) }
  $svcFile = Join-Path $d '.service'
  if (Test-Path $svcFile) {
    $svc = (Get-Content $svcFile | Select-Object -First 1).Trim()
    kv "$p.service" $svc
    $s = Get-Service -Name $svc
    if ($s) { kv "$p.service_state" ($(if ($s.Status -eq 'Running') { 'active' } else { 'inactive' })) } else { kv "$p.service_state" 'unknown' }
  }
  $i++
}

# --- The machine --------------------------------------------------------------
# Someone signed in to the desktop (Explorer runs in their session): there's a screen to show.
if (Get-Process explorer -ErrorAction SilentlyContinue | Where-Object { $_.SessionId -ne 0 }) { kv 'desktop' 'yes' } else { kv 'desktop' 'no' }
# A battery means a laptop (PCSystemType 2 is "mobile").
$cs = Get-CimInstance Win32_ComputerSystem
if ($cs) { kv 'form_factor' ($(if ($cs.PCSystemType -eq 2) { 'laptop' } else { 'desktop' })) }
# An administrator's ssh session is elevated: like passwordless sudo.
$me = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
if ($me.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) { kv 'sudo' 'passwordless' } else { kv 'sudo' 'password' }

# --- What it runs in, when it's virtual ------------------------------------------
$cs = Get-CimInstance Win32_ComputerSystem
$virt = switch -Regex ("$($cs.Manufacturer) $($cs.Model)") {
  'QEMU' { 'qemu'; break }
  'VMware' { 'vmware'; break }
  'VirtualBox|innotek' { 'oracle'; break }
  'Parallels' { 'parallels'; break }
  'Xen' { 'xen'; break }
  'Microsoft Corporation Virtual Machine' { 'microsoft'; break }
}
if ($virt) { kv 'virt' $virt }

# --- Docker -------------------------------------------------------------------
if (Get-Command docker -ErrorAction SilentlyContinue) {
  $dv = (& docker version --format '{{.Server.Version}}' 2>$null | Select-Object -First 1)
  if ($dv) { kv 'docker' $dv }
  else { kv 'docker_error' ((& docker version 2>&1 | Out-String) -split "`n" | Where-Object { $_ -match 'error|cannot|denied' } | Select-Object -First 1) }
}

# --- The network card it's reached by (to wake it later), and what wants a look -
$route = Get-NetRoute -DestinationPrefix '0.0.0.0/0' | Sort-Object { $_.RouteMetric + $_.InterfaceMetric } | Select-Object -First 1
if ($route) {
  $nic = Get-NetAdapter -InterfaceIndex $route.ifIndex
  if ($nic -and $nic.MacAddress) { kv 'mac' ($nic.MacAddress.Replace('-', ':').ToLower()) }
  $ip = Get-NetIPAddress -InterfaceIndex $route.ifIndex -AddressFamily IPv4 | Where-Object { $_.PrefixOrigin -ne 'WellKnown' } | Select-Object -First 1
  if ($ip) {
    $bytes = ([Net.IPAddress]$ip.IPAddress).GetAddressBytes()
    $bits = [int]$ip.PrefixLength
    $brd = for ($k = 0; $k -lt 4; $k++) {
      $keep = [Math]::Max(0, [Math]::Min(8, $bits - 8 * $k))
      $bytes[$k] -bor (255 -shr $keep)
    }
    kv 'lan' ("{0}/{1} {2}" -f $ip.IPAddress, $bits, ($brd -join '.'))
  }
}
# Updates waiting for a restart.
$pending = (Test-Path 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\WindowsUpdate\Auto Update\RebootRequired') -or
  (Test-Path 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Component Based Servicing\RebootPending')
if ($pending) { kv 'reboot_required' 'yes' }
# Services that should run (automatic, not delayed) but stopped with an error.
$failed = @(Get-CimInstance Win32_Service -Filter "StartMode='Auto' AND State='Stopped'" |
  Where-Object { $_.ExitCode -ne 0 -and $_.ExitCode -ne 1077 -and -not $_.DelayedAutoStart } | Select-Object -ExpandProperty Name)
if ($failed.Count -gt 0) { kv 'failed_units' ($failed -join ' ') }

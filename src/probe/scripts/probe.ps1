# Muxen machine probe (Windows). Read-only.
# Prints key=value lines; runner N is reported as rN.<key>=<value>.
# $OR_DIRS (array of glob patterns) is prepended by Muxen.

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

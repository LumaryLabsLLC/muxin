# OpenRunner: install GitHub Actions runners (Windows).
# OpenRunner prepends: $OR_VERSION, $OR_URL, $OR_TOKEN, $OR_NAMES (array),
# $OR_LABELS, $OR_GROUP, $OR_SERVICE (bool), $OR_BASE (empty: C:\actions-runner).

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
trap { Write-Error ("error: " + $_.Exception.Message); exit 1 }
function say($m) { Write-Output $m }

$arch = switch ($env:PROCESSOR_ARCHITECTURE) {
  'AMD64' { 'x64' }
  'ARM64' { 'arm64' }
  default { throw "Unsupported CPU architecture: $env:PROCESSOR_ARCHITECTURE" }
}
$base = if ($OR_BASE) { $OR_BASE } else { "$env:SystemDrive\actions-runner" }
$pkg = "actions-runner-win-$arch-$OR_VERSION.zip"
$url = "https://github.com/actions/runner/releases/download/v$OR_VERSION/$pkg"
$cache = Join-Path $base '.cache'

if ($OR_SERVICE) {
  $principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
  if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw "Installing a Windows service needs an administrator account. Connect as an admin, or turn off ""Install as a service""."
  }
}
foreach ($name in $OR_NAMES) {
  if (Test-Path (Join-Path (Join-Path $base $name) '.runner')) {
    throw "$base\$name already has a configured runner. Remove it first or pick another name."
  }
}

New-Item -ItemType Directory -Force -Path $cache | Out-Null
$zip = Join-Path $cache $pkg
if (Test-Path $zip) {
  say "Using cached $pkg"
} else {
  say "Downloading actions-runner $OR_VERSION for win-$arch…"
  [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
  Invoke-WebRequest -Uri $url -OutFile "$zip.part" -UseBasicParsing
  Move-Item "$zip.part" $zip -Force
}

foreach ($name in $OR_NAMES) {
  $dir = Join-Path $base $name
  say "Setting up $name in $dir"
  New-Item -ItemType Directory -Force -Path $dir | Out-Null
  Expand-Archive -Path $zip -DestinationPath $dir -Force
  Push-Location $dir
  $cfg = @('--unattended', '--url', $OR_URL, '--token', $OR_TOKEN, '--name', $name, '--work', '_work', '--replace')
  if ($OR_LABELS) { $cfg += @('--labels', $OR_LABELS) }
  if ($OR_GROUP) { $cfg += @('--runnergroup', $OR_GROUP) }
  if ($OR_SERVICE) { $cfg += '--runasservice' }
  & .\config.cmd @cfg
  if ($LASTEXITCODE -ne 0) { throw "config.cmd failed for $name (exit $LASTEXITCODE)" }
  if (-not $OR_SERVICE) {
    Start-Process -FilePath (Join-Path $dir 'run.cmd') -WorkingDirectory $dir -WindowStyle Hidden
    say "Started $name in the background (not a service, so it won't come back after a reboot)"
  }
  Pop-Location
  say "✓ $name is installed"
}

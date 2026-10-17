# Muxin: install GitHub Actions runners (Windows).
# Muxin prepends: $OR_VERSION, $OR_SHA256 (array of "<platform>=<hex>": each
# package's SHA-256 from the release notes), $OR_URL, $OR_TOKEN, $OR_NAMES (array),
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

# The package has to match the SHA-256 GitHub published for it, downloaded or cached.
$sha = $null
foreach ($entry in $OR_SHA256) {
  if ($entry.StartsWith("win-$arch=", [StringComparison]::Ordinal)) { $sha = $entry.Substring("win-$arch=".Length) }
}
if (-not $sha) { throw "GitHub's release notes for actions-runner $OR_VERSION don't list a SHA-256 for $pkg, so it can't be checked. Not installing it." }
function Get-Sha256($path) { (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant() }

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

# Only Administrators, SYSTEM and this account may change the runner folders.
# C:\ lets every signed-in user change the folders made in it, and the runners
# (and config.cmd, as an admin) run what's in them.
$me = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$trusted = @('S-1-5-32-544', 'S-1-5-18', $me)
function Get-Owner($path) { (Get-Acl -LiteralPath $path).GetOwner([Security.Principal.SecurityIdentifier]).Value }
# Refuses a folder another account owns. -NoLink: also one that's a link, which
# Muxin never makes (it could lead somewhere others can change).
function Assert-Owner($path, [switch]$NoLink) {
  $item = Get-Item -LiteralPath $path -Force -ErrorAction SilentlyContinue
  if (-not $item) { return }
  if ($NoLink -and $item.Attributes.HasFlag([IO.FileAttributes]::ReparsePoint)) {
    throw "$path is a link to another folder, which Muxin doesn't make. Check it, delete it and try again."
  }
  $owner = try { Get-Owner $path } catch { throw "Couldn't see who owns $path. Check what's in it, delete it and try again." }
  if ($trusted -notcontains $owner) {
    $who = try { (New-Object Security.Principal.SecurityIdentifier $owner).Translate([Security.Principal.NTAccount]).Value } catch { $owner }
    throw "$path belongs to $who, not to Administrators, SYSTEM or $env:USERNAME, so that account could change what the runners run. Check what's in it, then delete it or make Administrators its owner (icacls ""$path"" /setowner *S-1-5-32-544 /t) and try again."
  }
}
# A cached package is used only when one of those three made it and it checks out.
function Test-Cached($path) {
  try { ($trusted -contains (Get-Owner $path)) -and ((Get-Sha256 $path) -eq $sha) } catch { $false }
}
$fresh = -not (Test-Path -LiteralPath $base)
New-Item -ItemType Directory -Force -Path $base | Out-Null
Assert-Owner $base
# Nothing inherited from C:\, and full control for those three only. With
# --runasservice, config.cmd gives the service's account (NETWORK SERVICE) full
# control of its own runner folder and _work, through a GITHUB_ActionsRunner_G* group.
& icacls.exe $base /inheritance:r /grant:r '*S-1-5-32-544:(OI)(CI)F' '*S-1-5-18:(OI)(CI)F' "*$($me):(OI)(CI)F" | Out-Null
if ($LASTEXITCODE -ne 0) { throw "Couldn't limit who can change $base (icacls exit $LASTEXITCODE). Install as an administrator, or as the account that owns it." }
# Anything that showed up before the line above belongs to someone else.
if ($fresh -and (Get-ChildItem -LiteralPath $base -Force)) {
  throw "Something was put in $base while it was being made. Check what's in it, delete it and try again."
}
Assert-Owner $cache -NoLink
foreach ($name in $OR_NAMES) { Assert-Owner (Join-Path $base $name) -NoLink }

New-Item -ItemType Directory -Force -Path $cache | Out-Null
$zip = Join-Path $cache $pkg
if ((Test-Path -LiteralPath $zip) -and (Test-Cached $zip)) {
  say "Using cached $pkg"
} else {
  if (Test-Path -LiteralPath $zip) {
    say "The cached $pkg doesn't match its SHA-256 or belongs to another account; downloading it again."
    Remove-Item -LiteralPath $zip -Force
  }
  # A new file, not one someone else made: its owner could still change it.
  Remove-Item -LiteralPath "$zip.part" -Force -ErrorAction SilentlyContinue
  say "Downloading actions-runner $OR_VERSION for win-$arch…"
  [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
  Invoke-WebRequest -Uri $url -OutFile "$zip.part" -UseBasicParsing
  $got = Get-Sha256 "$zip.part"
  if ($got -ne $sha) {
    Remove-Item -LiteralPath "$zip.part" -Force
    throw "The downloaded $pkg doesn't match the SHA-256 GitHub published for it (expected $sha, got $got), so it was deleted. Try again. If it keeps happening, something between this machine and GitHub is changing the download."
  }
  Move-Item -LiteralPath "$zip.part" -Destination $zip -Force
}
say "Checked $pkg against GitHub's SHA-256"

foreach ($name in $OR_NAMES) {
  $dir = Join-Path $base $name
  say "Setting up $name in $dir"
  New-Item -ItemType Directory -Force -Path $dir | Out-Null
  Expand-Archive -Path $zip -DestinationPath $dir -Force
  Push-Location $dir
  $cfg = @('--unattended', '--url', $OR_URL, '--name', $name, '--work', '_work', '--replace')
  if ($OR_LABELS) { $cfg += @('--labels', $OR_LABELS) }
  if ($OR_GROUP) { $cfg += @('--runnergroup', $OR_GROUP) }
  if ($OR_SERVICE) { $cfg += '--runasservice' }
  # The token goes in config.cmd's environment, not on its command line, which
  # other accounts can read.
  $env:ACTIONS_RUNNER_INPUT_TOKEN = $OR_TOKEN
  try { & .\config.cmd @cfg } finally { Remove-Item Env:\ACTIONS_RUNNER_INPUT_TOKEN -ErrorAction SilentlyContinue }
  if ($LASTEXITCODE -ne 0) { throw "config.cmd failed for $name (exit $LASTEXITCODE)" }
  if (-not $OR_SERVICE) {
    Start-Process -FilePath (Join-Path $dir 'run.cmd') -WorkingDirectory $dir -WindowStyle Hidden
    say "Started $name in the background (not a service, so it won't come back after a reboot)"
  }
  Pop-Location
  say "✓ $name is installed"
}

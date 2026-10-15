# Builds Muxen for Windows and packs it: dist\Muxen-<version>-windows-<arch>-setup.exe
# (an installer, with Inno Setup) and dist\Muxen-<version>-windows-<arch>.zip, both
# with muxen.exe and, beside it in agents\, the agent builds it sets up on each
# machine (made on a Mac or Linux by scripts/build-agents.sh). <arch> is what
# Rust builds for here: x64, or arm64 on Windows on Arm.
#
# Needs: Rust, Visual Studio's C++ build tools and the Windows SDK (for
# GPUI's shaders and the icon).
param([switch]$SkipBuild)
$ErrorActionPreference = 'Stop'
Set-Location (Split-Path $PSScriptRoot -Parent)
$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
$arch = if ((rustc -vV | Select-String '^host: (.+)').Matches[0].Groups[1].Value -like 'aarch64-*') { 'arm64' } else { 'x64' }
# GPUI compiles its shaders with the Windows SDK's fxc.exe.
if (-not $env:GPUI_FXC_PATH -and -not (Get-Command fxc.exe -ErrorAction SilentlyContinue)) {
  # This machine's own build first (the x64 one also runs on Arm), newest SDK first.
  $fxc = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin" -Recurse -Filter fxc.exe -ErrorAction SilentlyContinue |
    Where-Object { $_.FullName -match '\\(x64|arm64)\\' } |
    Sort-Object @{ Expression = { $_.FullName -match "\\$arch\\" }; Descending = $true }, @{ Expression = 'FullName'; Descending = $true } |
    Select-Object -First 1
  if (-not $fxc) { throw "The Windows SDK's fxc.exe isn't installed (Visual Studio Installer > Windows SDK)." }
  $env:GPUI_FXC_PATH = $fxc.FullName
}
if (-not $SkipBuild) {
  cargo build --release --locked
  if ($LASTEXITCODE) { exit $LASTEXITCODE }
}
$agents = @(Get-ChildItem dist\agents -Filter 'muxen-agent-*' -ErrorAction SilentlyContinue)
if ($agents.Count -eq 0) { throw 'No agent builds in dist\agents: run scripts/build-agents.sh on a Mac or Linux first.' }
$root = 'dist\windows'
$out = Join-Path $root 'Muxen'
Remove-Item -Recurse -Force $root -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force (Join-Path $out 'agents') | Out-Null
Copy-Item target\release\muxen.exe $out
$agents | Copy-Item -Destination (Join-Path $out 'agents')
Copy-Item README.md, LICENSE, THIRD_PARTY_NOTICES.md $out
$zip = "dist\Muxen-$version-windows-$arch.zip"
Compress-Archive -Path $out -DestinationPath $zip -Force
"Built $zip ($([math]::Round((Get-Item $zip).Length / 1MB, 1)) MB)"

# The installer, when Inno Setup is here (winget install JRSoftware.InnoSetup).
$iscc = @(Get-Command ISCC.exe -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Source) +
  @("$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe", "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe") | Where-Object { $_ -and (Test-Path $_) } | Select-Object -First 1
if ($iscc) {
  & $iscc /Q "/DAppVersion=$version" "/DArch=$arch" "/FMuxen-$version-windows-$arch-setup" scripts\muxen.iss
  if ($LASTEXITCODE) { exit $LASTEXITCODE }
  $setup = "dist\Muxen-$version-windows-$arch-setup.exe"
  "Built $setup ($([math]::Round((Get-Item $setup).Length / 1MB, 1)) MB)"
} else {
  'No Inno Setup here, so no installer (winget install JRSoftware.InnoSetup).'
}

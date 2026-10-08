# Runs one command in a workspace (Windows, best effort). Prepended: $OR_DIR $OR_CWD
# $OR_CMD $OR_TIMEOUT $OR_BG $OR_LOG $OR_ID and `$env:NAME = ...` lines.
# Timeouts are enforced by Muxen hanging up; the command isn't killed on the machine.

$ErrorActionPreference = 'Continue'
function Or-Fail($msg) { [Console]::Error.WriteLine("error: $msg"); [Console]::Error.WriteLine("${OR_MARK}:exit=97"); exit 0 }
try { Set-Location -LiteralPath $OR_DIR -ErrorAction Stop } catch { Or-Fail "the workspace folder is gone: $OR_DIR" }
if ($OR_CWD) { try { Set-Location -LiteralPath $OR_CWD -ErrorAction Stop } catch { Or-Fail "there's no folder $OR_CWD in the workspace" } }
$env:MUXEN_WORKSPACE = $OR_ID
$env:NO_COLOR = '1'
$env:GIT_TERMINAL_PROMPT = '0'

if ($OR_BG) {
  New-Item -ItemType Directory -Force -Path (Join-Path $OR_DIR '.muxen\logs') | Out-Null
  $log = Join-Path $OR_DIR $OR_LOG
  $p = Start-Process -FilePath 'powershell' -ArgumentList @('-NoProfile', '-NonInteractive', '-Command', $OR_CMD) `
    -RedirectStandardOutput $log -RedirectStandardError "$log.err" -WindowStyle Hidden -PassThru
  Write-Output "pid=$($p.Id)"
  [Console]::Error.WriteLine("${OR_MARK}:exit=0")
  exit 0
}

$global:LASTEXITCODE = 0
$code = 0
try {
  Invoke-Expression $OR_CMD
  if (-not $?) { $code = 1 }
  if ($LASTEXITCODE) { $code = $LASTEXITCODE }
} catch {
  [Console]::Error.WriteLine($_.ToString())
  $code = 1
}
[Console]::Error.WriteLine("${OR_MARK}:exit=$code")
exit 0

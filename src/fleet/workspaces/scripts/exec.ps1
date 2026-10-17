# Runs one command in a workspace (Windows). Prepended: $OR_DIR $OR_CWD $OR_CMD
# $OR_TIMEOUT $OR_BG $OR_LOG $OR_ID $OR_ENVPS and `$env:NAME = ...` lines.
# The command runs as a script of its own in its own PowerShell, so its output
# is text as in a terminal, its `exit` is its exit status, and a timeout can
# end it with everything it started.

$ErrorActionPreference = 'Continue'
function Or-Fail($msg) { [Console]::Error.WriteLine("error: $msg"); [Console]::Error.WriteLine("${OR_MARK}:exit=97"); exit 0 }
try { Set-Location -LiteralPath $OR_DIR -ErrorAction Stop } catch { Or-Fail "the workspace folder is gone: $OR_DIR" }
if ($OR_CWD) { try { Set-Location -LiteralPath $OR_CWD -ErrorAction Stop } catch { Or-Fail "there's no folder $OR_CWD in the workspace" } }
$here = (Get-Location).Path
$env:MUXEN_WORKSPACE = $OR_ID
$env:NO_COLOR = '1'
$env:GIT_TERMINAL_PROMPT = '0'

$runs = Join-Path $OR_DIR '.muxen'
New-Item -ItemType Directory -Force -Path (Join-Path $runs 'logs') | Out-Null
$f = Join-Path $runs ('run-' + [guid]::NewGuid().ToString('N').Substring(0, 8) + '.ps1')
# A string literal: PowerShell takes U+2018, U+2019, U+201A and U+201B for
# single quotes too, so all five are doubled (as \u escapes: this file stays ASCII).
$q = { param($s) "'" + ($s -replace '[\u0027\u2018\u2019\u201A\u201B]', '$0$0') + "'" }

if ($OR_BG) {
  # Started through WMI, so it outlives this ssh session (sshd ends what a
  # session started); WMI passes no environment, so the script sets it up.
  $log = Join-Path $OR_DIR $OR_LOG
  $body = "[Console]::OutputEncoding = [Text.Encoding]::UTF8`n" + $OR_ENVPS +
    "`$env:MUXEN_WORKSPACE = $(& $q $OR_ID)`n`$env:NO_COLOR = '1'`n`$env:GIT_TERMINAL_PROMPT = '0'`n" +
    "Set-Location -LiteralPath $(& $q $here)`n" + $OR_CMD
  Set-Content -LiteralPath $f -Value $body -Encoding UTF8
  $line = "cmd.exe /d /c powershell -NoProfile -NonInteractive -ExecutionPolicy Bypass -File `"$f`" > `"$log`" 2>&1"
  $r = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{ CommandLine = $line; CurrentDirectory = $here }
  if ($r.ReturnValue -ne 0) { Or-Fail "Windows didn't start it ($($r.ReturnValue))" }
  Write-Output "pid=$($r.ProcessId)"
  [Console]::Error.WriteLine("${OR_MARK}:exit=0")
  exit 0
}

Set-Content -LiteralPath $f -Value $OR_CMD -Encoding UTF8
$out = "$f.out"; $err = "$f.err"
$run = "[Console]::OutputEncoding = [Text.Encoding]::UTF8; & $(& $q $f); if (`$LASTEXITCODE) { exit `$LASTEXITCODE } elseif (-not `$?) { exit 1 } else { exit 0 }"
$p = Start-Process -FilePath 'powershell' -ArgumentList @('-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-Command', "`"$($run.Replace('"', '\"'))`"") `
  -NoNewWindow -PassThru -RedirectStandardOutput $out -RedirectStandardError $err
$null = $p.Handle
$done = $p.WaitForExit([int]$OR_TIMEOUT * 1000)
if (-not $done) {
  taskkill /T /F /PID $p.Id 2>&1 | Out-Null
  [Console]::Error.WriteLine("${OR_MARK}:timeout")
}
$utf8 = New-Object Text.UTF8Encoding $false
if (Test-Path -LiteralPath $out) { [Console]::Out.Write([IO.File]::ReadAllText($out, $utf8)) }
if (Test-Path -LiteralPath $err) { [Console]::Error.Write([IO.File]::ReadAllText($err, $utf8)) }
$code = if ($done) { $p.ExitCode } else { 124 }
Remove-Item -LiteralPath $f, $out, $err -Force -ErrorAction SilentlyContinue
[Console]::Error.WriteLine("${OR_MARK}:exit=$code")
exit 0

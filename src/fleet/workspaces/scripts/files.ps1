# Reads, writes or lists files in a workspace (Windows, best effort).
# Prepended: $OR_ACTION $OR_DIR $OR_REL $OR_MAX $OR_APPEND $OR_B64 $OR_DEPTH

$ErrorActionPreference = 'Stop'
try { Set-Location -LiteralPath $OR_DIR } catch { [Console]::Error.WriteLine("error: the workspace folder is gone: $OR_DIR"); exit 97 }
$full = if ($OR_REL) { Join-Path $OR_DIR $OR_REL } else { $OR_DIR }

switch ($OR_ACTION) {
  'read' {
    if (-not (Test-Path -LiteralPath $full)) { [Console]::Error.WriteLine("error: there's no file $OR_REL in the workspace"); exit 3 }
    $item = Get-Item -LiteralPath $full -Force
    if ($item.PSIsContainer) { [Console]::Error.WriteLine("error: $OR_REL is a folder"); exit 3 }
    Write-Output "size=$($item.Length)"
    # Shared, so a log a command is still writing can be read.
    $stream = [IO.File]::Open($full, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::ReadWrite -bor [IO.FileShare]::Delete)
    try {
      $count = [Math]::Min([int64]$OR_MAX, $item.Length)
      $buf = New-Object byte[] $count
      $read = $stream.Read($buf, 0, $count)
      Write-Output ("data=" + [Convert]::ToBase64String($buf, 0, $read))
    } finally { $stream.Dispose() }
  }
  'write' {
    $parent = Split-Path -Parent $full
    if ($parent) { New-Item -ItemType Directory -Force -Path $parent | Out-Null }
    $bytes = [Convert]::FromBase64String($OR_B64)
    $mode = if ($OR_APPEND) { [IO.FileMode]::Append } else { [IO.FileMode]::Create }
    $stream = New-Object IO.FileStream($full, $mode)
    try { $stream.Write($bytes, 0, $bytes.Length) } finally { $stream.Dispose() }
    Write-Output "size=$((Get-Item -LiteralPath $full -Force).Length)"
  }
  'list' {
    if (-not (Test-Path -LiteralPath $full -PathType Container)) { [Console]::Error.WriteLine("error: there's no folder $OR_REL in the workspace"); exit 3 }
    $root = (Get-Item -LiteralPath $full -Force).FullName.TrimEnd('\') + '\'
    Get-ChildItem -LiteralPath $full -Recurse -Depth ([int]$OR_DEPTH - 1) -Force |
      Where-Object { $_.FullName -notmatch '\\(\.git|\.muxen)(\\|$)' } |
      Select-Object -First 2001 |
      ForEach-Object {
        $kind = if ($_.LinkType) { 'l' } elseif ($_.PSIsContainer) { 'd' } else { 'f' }
        $size = if ($_.PSIsContainer) { 0 } else { $_.Length }
        $rel = $_.FullName.Substring($root.Length).Replace('\', '/')
        Write-Output "$kind`t$size`t$rel"
      }
  }
}

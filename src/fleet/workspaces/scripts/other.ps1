# Create, changes and destroy for Windows workspaces (best effort). $OR_ACTION picks one.
# create:  $OR_ROOT $OR_ID $OR_REPO $OR_REPO_SHOWN $OR_REF $OR_BRANCH $OR_AUTH
# changes: $OR_DIR
# destroy: $OR_DIR $OR_ID $OR_PIDS $OR_KEEP

$ErrorActionPreference = 'Continue'
$env:GIT_TERMINAL_PROMPT = '0'
$env:GIT_PAGER = 'cat'

switch ($OR_ACTION) {
  'create' {
    $root = $OR_ROOT -replace '^~', $HOME
    $dir = Join-Path $root $OR_ID
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    Set-Location -LiteralPath $dir
    Write-Output "path=$((Get-Location).Path)"
    if ($OR_REPO) {
      if (-not (Get-Command git -ErrorAction SilentlyContinue)) { [Console]::Error.WriteLine("error: git isn't installed on $env:COMPUTERNAME"); exit 3 }
      if ($OR_AUTH) { $env:GIT_CONFIG_COUNT = '1'; $env:GIT_CONFIG_KEY_0 = 'http.extraHeader'; $env:GIT_CONFIG_VALUE_0 = $OR_AUTH }
      Get-ChildItem -Force | Remove-Item -Recurse -Force -ErrorAction SilentlyContinue
      Write-Output "Cloning $OR_REPO_SHOWN"
      if ($OR_REF) {
        git clone --depth 1 --branch $OR_REF $OR_REPO . 2>&1 | ForEach-Object { "$_" }
        if ($LASTEXITCODE -ne 0) {
          Get-ChildItem -Force | Remove-Item -Recurse -Force -ErrorAction SilentlyContinue
          git clone $OR_REPO . 2>&1 | ForEach-Object { "$_" }
          if ($LASTEXITCODE -ne 0) { [Console]::Error.WriteLine('error: git clone failed'); exit 4 }
          git checkout $OR_REF 2>&1 | ForEach-Object { "$_" }
          if ($LASTEXITCODE -ne 0) { [Console]::Error.WriteLine("error: couldn't check out $OR_REF"); exit 5 }
        }
      } else {
        git clone --depth 1 $OR_REPO . 2>&1 | ForEach-Object { "$_" }
        if ($LASTEXITCODE -ne 0) { [Console]::Error.WriteLine('error: git clone failed'); exit 4 }
      }
      if ($OR_BRANCH) {
        git checkout -b $OR_BRANCH 2>&1 | ForEach-Object { "$_" }
        if ($LASTEXITCODE -ne 0) { [Console]::Error.WriteLine("error: couldn't create branch $OR_BRANCH"); exit 5 }
      }
      Add-Content -Path '.git\info\exclude' -Value '.muxen/'
    }
    New-Item -ItemType Directory -Force -Path '.muxen\logs' | Out-Null
    Write-Output 'ready'
  }
  'changes' {
    try { Set-Location -LiteralPath $OR_DIR -ErrorAction Stop } catch { [Console]::Error.WriteLine("error: the workspace folder is gone: $OR_DIR"); exit 97 }
    if (-not (Get-Command git -ErrorAction SilentlyContinue)) { Write-Output "git isn't installed on this machine, so there's no change tracking."; exit 0 }
    git rev-parse --is-inside-work-tree 2>$null | Out-Null
    if ($LASTEXITCODE -ne 0) { Write-Output "This workspace isn't a git checkout, so there's nothing to diff."; exit 0 }
    Write-Output '## git status'
    git status --short --branch
    Write-Output ''
    Write-Output '## git diff'
    git --no-pager diff --no-color HEAD
  }
  'destroy' {
    foreach ($p in ($OR_PIDS -split ' ' | Where-Object { $_ })) {
      if (Get-Process -Id $p -ErrorAction SilentlyContinue) { taskkill /PID $p /T /F | Out-Null; Write-Output "Stopped background process $p" }
    }
    if ($OR_KEEP) { Write-Output "Kept the files in $OR_DIR"; exit 0 }
    if (-not $OR_DIR.TrimEnd('\').EndsWith("\$OR_ID")) { [Console]::Error.WriteLine("error: refusing to delete $OR_DIR"); exit 3 }
    if (Test-Path -LiteralPath $OR_DIR) { Remove-Item -LiteralPath $OR_DIR -Recurse -Force; Write-Output "Deleted $OR_DIR" } else { Write-Output "$OR_DIR was already gone" }
  }
}

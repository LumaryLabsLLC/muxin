# Create, changes and destroy for Windows workspaces (best effort). $OR_ACTION picks one.
# create:   $OR_ROOT $OR_ID $OR_REPO $OR_REPO_SHOWN $OR_REF $OR_BRANCH $OR_AUTH
# worktree: $OR_ROOT $OR_ID $OR_SRC $OR_REF $OR_BRANCH (a git worktree of the checkout $OR_SRC; see worktree.sh)
# changes:  $OR_DIR
# destroy:  $OR_DIR $OR_ID $OR_PIDS $OR_KEEP $OR_SRC

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
  'worktree' {
    $root = $OR_ROOT -replace '^~', $HOME
    $src = $OR_SRC -replace '^~', $HOME
    if (-not (Get-Command git -ErrorAction SilentlyContinue)) { [Console]::Error.WriteLine("error: git isn't installed on $env:COMPUTERNAME"); exit 3 }
    git -C $src rev-parse --git-dir 2>$null | Out-Null
    if ($LASTEXITCODE -ne 0) { [Console]::Error.WriteLine("error: $OR_SRC isn't a git checkout"); exit 3 }
    New-Item -ItemType Directory -Force -Path $root | Out-Null
    $dir = Join-Path (Resolve-Path -LiteralPath $root).Path $OR_ID
    if (Test-Path -LiteralPath $dir) { [Console]::Error.WriteLine("error: $dir is already there"); exit 3 }
    Write-Output "path=$dir"
    Write-Output "Fetching in $OR_SRC"
    git -C $src fetch --quiet origin 2>&1 | ForEach-Object { "$_" }
    $base = $OR_REF
    if ($base -match '^pull/(\d+)/head$') {
      $pr = "pr-$($Matches[1])"
      git -C $src fetch --quiet origin "+refs/$($base):refs/heads/$pr" 2>&1 | ForEach-Object { "$_" }
      if ($LASTEXITCODE -ne 0) { [Console]::Error.WriteLine("error: couldn't fetch $base"); exit 5 }
      $base = $pr
    } elseif (-not $base) {
      $base = git -C $src symbolic-ref --quiet --short refs/remotes/origin/HEAD 2>$null
      if (-not $base) { $base = 'HEAD' }
    }
    $has = { param($ref) git -C $src show-ref --verify --quiet $ref; $LASTEXITCODE -eq 0 }
    if ($OR_BRANCH) {
      if (& $has "refs/heads/$OR_BRANCH") { git -C $src worktree add $dir $OR_BRANCH 2>&1 | ForEach-Object { "$_" } }
      else { git -C $src worktree add --no-track -b $OR_BRANCH $dir $base 2>&1 | ForEach-Object { "$_" } }
    } elseif (& $has "refs/heads/$base") {
      git -C $src worktree add $dir $base 2>&1 | ForEach-Object { "$_" }
    } elseif (& $has "refs/remotes/origin/$base") {
      git -C $src worktree add --track -b $base $dir "origin/$base" 2>&1 | ForEach-Object { "$_" }
    } else {
      git -C $src worktree add --detach $dir $base 2>&1 | ForEach-Object { "$_" }
    }
    if ($LASTEXITCODE -ne 0) { [Console]::Error.WriteLine("error: couldn't make the worktree"); exit 5 }
    Set-Location -LiteralPath $dir
    New-Item -ItemType Directory -Force -Path '.muxen\logs' | Out-Null
    $exclude = Join-Path (git rev-parse --git-common-dir) 'info\exclude'
    if (-not (Select-String -Path $exclude -Pattern '^\.muxen/$' -Quiet -ErrorAction SilentlyContinue)) { Add-Content -Path $exclude -Value '.muxen/' }
    Write-Output "Worktree of $OR_SRC on $(git rev-parse --abbrev-ref HEAD) at $(git rev-parse --short HEAD)"
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
    if ($OR_SRC) {
      $src = $OR_SRC -replace '^~', $HOME
      git -C $src worktree remove --force $OR_DIR 2>$null
      if ($LASTEXITCODE -eq 0) { Write-Output "Removed the worktree from $OR_SRC (its branch stays)" }
      git -C $src worktree prune 2>$null
    }
    if (-not $OR_DIR.TrimEnd('\').EndsWith("\$OR_ID")) { [Console]::Error.WriteLine("error: refusing to delete $OR_DIR"); exit 3 }
    if (Test-Path -LiteralPath $OR_DIR) { Remove-Item -LiteralPath $OR_DIR -Recurse -Force; Write-Output "Deleted $OR_DIR" } else { Write-Output "$OR_DIR was already gone" }
  }
}

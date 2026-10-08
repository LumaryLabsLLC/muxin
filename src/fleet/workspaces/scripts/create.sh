# Creates a workspace folder and optionally clones a repository into it.
# Prepended: OR_ROOT OR_ID OR_STEP (all | clone) OR_REPO OR_REPO_SHOWN OR_REF OR_BRANCH OR_AUTH
# OR_AUTH is an http.extraHeader value; it only ever lives in this process's environment.

or_root=$OR_ROOT
case "$or_root" in "~") or_root=$HOME ;; "~/"*) or_root="$HOME/${or_root#\~/}" ;; esac
mkdir -p "$or_root/$OR_ID" || { echo "error: couldn't create $or_root/$OR_ID" >&2; exit 3; }
cd "$or_root/$OR_ID" || { echo "error: couldn't open $or_root/$OR_ID" >&2; exit 3; }
or_here=$(pwd -P)
case "$or_here" in */"$OR_ID") ;; *) echo "error: unexpected workspace folder $or_here" >&2; exit 3 ;; esac
printf 'path=%s\n' "$or_here"

if [ -n "$OR_REPO" ]; then
  command -v git >/dev/null 2>&1 || { echo "error: git isn't installed on $(hostname)" >&2; exit 3; }
  export GIT_TERMINAL_PROMPT=0
  if [ -n "$OR_AUTH" ]; then
    export GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=http.extraHeader GIT_CONFIG_VALUE_0="$OR_AUTH"
  fi
  # Start from an empty folder (a retry may follow a failed attempt).
  find . -mindepth 1 -maxdepth 1 -exec rm -rf {} + 2>/dev/null
  echo "Cloning $OR_REPO_SHOWN"
  if [ -n "$OR_REF" ]; then
    # Branches and tags clone shallow; a commit needs history.
    if ! git clone --progress --depth 1 --branch "$OR_REF" "$OR_REPO" . 2>&1; then
      find . -mindepth 1 -maxdepth 1 -exec rm -rf {} + 2>/dev/null
      echo "$OR_REF isn't a branch or tag; cloning full history to check it out"
      git clone --progress "$OR_REPO" . 2>&1 || { echo "error: git clone failed" >&2; exit 4; }
      git -c advice.detachedHead=false checkout "$OR_REF" 2>&1 || { echo "error: couldn't check out $OR_REF" >&2; exit 5; }
    fi
  else
    git clone --progress --depth 1 "$OR_REPO" . 2>&1 || { echo "error: git clone failed" >&2; exit 4; }
  fi
  if [ -n "$OR_BRANCH" ]; then
    git checkout -b "$OR_BRANCH" 2>&1 || { echo "error: couldn't create branch $OR_BRANCH" >&2; exit 5; }
  fi
  printf '.openrunner/\n' >> .git/info/exclude 2>/dev/null
  echo "Checked out $(git rev-parse --abbrev-ref HEAD 2>/dev/null) at $(git rev-parse --short HEAD 2>/dev/null)"
fi
mkdir -p .openrunner/logs
echo "ready"

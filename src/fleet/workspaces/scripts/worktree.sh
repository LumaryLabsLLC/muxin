# Makes a workspace a git worktree of a checkout on this machine: its own
# folder and branch, sharing the checkout's history, so several agents can
# work on one repository at once.
# Prepended: OR_ROOT OR_ID OR_SRC (the checkout) OR_REF (what to start from;
# empty for the default branch) OR_BRANCH (a new branch; empty works on OR_REF)

or_home() { case "$1" in "~") printf '%s' "$HOME" ;; "~/"*) printf '%s/%s' "$HOME" "${1#\~/}" ;; *) printf '%s' "$1" ;; esac; }
or_root=$(or_home "$OR_ROOT")
or_src=$(or_home "$OR_SRC")
command -v git >/dev/null 2>&1 || { echo "error: git isn't installed on $(hostname)" >&2; exit 3; }
git -C "$or_src" rev-parse --git-dir >/dev/null 2>&1 || { echo "error: $OR_SRC isn't a git checkout" >&2; exit 3; }
mkdir -p "$or_root" || { echo "error: couldn't create $or_root" >&2; exit 3; }
or_dir="$(cd "$or_root" && pwd -P)/$OR_ID"
[ -e "$or_dir" ] && { echo "error: $or_dir is already there" >&2; exit 3; }
printf 'path=%s\n' "$or_dir"
export GIT_TERMINAL_PROMPT=0

# Start from the newest commits (offline, from what's there).
echo "Fetching in $OR_SRC"
git -C "$or_src" fetch --quiet origin 2>&1 || echo "Couldn't fetch, so it starts from what's on the machine"
or_base=$OR_REF
case "$or_base" in
  pull/*/head)
    # A pull request's code (from a fork too): GitHub's ref for it.
    or_pr=${or_base#pull/}; or_pr=pr-${or_pr%/head}
    git -C "$or_src" fetch --quiet origin "+refs/$or_base:refs/heads/$or_pr" 2>&1 || { echo "error: couldn't fetch $or_base" >&2; exit 5; }
    or_base=$or_pr ;;
  "")
    or_base=$(git -C "$or_src" symbolic-ref --quiet --short refs/remotes/origin/HEAD 2>/dev/null) || or_base=HEAD ;;
esac

if [ -n "$OR_BRANCH" ]; then
  if git -C "$or_src" show-ref --verify --quiet "refs/heads/$OR_BRANCH"; then
    git -C "$or_src" worktree add "$or_dir" "$OR_BRANCH" 2>&1 || { echo "error: couldn't make a worktree on $OR_BRANCH" >&2; exit 5; }
  else
    git -C "$or_src" worktree add --no-track -b "$OR_BRANCH" "$or_dir" "$or_base" 2>&1 || { echo "error: couldn't make a worktree from $or_base" >&2; exit 5; }
  fi
elif git -C "$or_src" show-ref --verify --quiet "refs/heads/$or_base"; then
  git -C "$or_src" worktree add "$or_dir" "$or_base" 2>&1 || { echo "error: couldn't make a worktree on $or_base" >&2; exit 5; }
elif git -C "$or_src" show-ref --verify --quiet "refs/remotes/origin/$or_base"; then
  # A branch only on GitHub (a pull request's): a local one that tracks it.
  git -C "$or_src" worktree add --track -b "$or_base" "$or_dir" "origin/$or_base" 2>&1 || { echo "error: couldn't make a worktree on $or_base" >&2; exit 5; }
else
  git -C "$or_src" worktree add --detach "$or_dir" "$or_base" 2>&1 || { echo "error: couldn't make a worktree at $or_base" >&2; exit 5; }
fi

cd "$or_dir" || { echo "error: couldn't open $or_dir" >&2; exit 3; }
mkdir -p .muxen/logs
# .muxen/ stays out of git: worktrees share the checkout's exclude file.
or_exclude="$(git rev-parse --git-common-dir)/info/exclude"
grep -qx '.muxen/' "$or_exclude" 2>/dev/null || printf '.muxen/\n' >> "$or_exclude"
echo "Worktree of $OR_SRC on $(git rev-parse --abbrev-ref HEAD) at $(git rev-parse --short HEAD)"
echo "ready"

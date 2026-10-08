# git status and the diff of uncommitted work. Prepended: OR_DIR
cd "$OR_DIR" 2>/dev/null || { echo "error: the workspace folder is gone: $OR_DIR" >&2; exit 97; }
export GIT_PAGER=cat PAGER=cat NO_COLOR=1
if ! command -v git >/dev/null 2>&1; then echo "git isn't installed on this machine, so there's no change tracking."; exit 0; fi
if ! git rev-parse --is-inside-work-tree >/dev/null 2>&1; then echo "This workspace isn't a git checkout, so there's nothing to diff."; exit 0; fi
echo "## git status"
git status --short --branch
echo
echo "## git diff"
if git rev-parse --verify -q HEAD >/dev/null 2>&1; then git --no-pager diff --no-color HEAD; else git --no-pager diff --no-color --cached; fi

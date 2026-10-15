#!/bin/sh
# Starts a release: sets the app's version in Cargo.toml (and Cargo.lock),
# commits that, and tags the commit v<version>. Pushing the tag starts the
# release workflow, which builds every platform into a draft GitHub release.
#
#   scripts/release.sh 0.2.0           # commit and tag here; push yourself
#   scripts/release.sh 0.2.0 --push    # and push the branch and the tag
#
# Run on another branch that main follows (like muxen), --push moves main up to
# it too, here and on GitHub, when main can fast-forward.
#
# See RELEASING.md.
set -eu

cd "$(dirname "$0")/.."
usage="usage: scripts/release.sh <version> [--push]"
version=${1:?$usage}
version=${version#v}
push=
case "${2:-}" in
    --push) push=1 ;;
    "") ;;
    *) echo "$usage" >&2; exit 2 ;;
esac

echo "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$' \
    || { echo "error: \"$version\" isn't a version like 1.2.3" >&2; exit 2; }
[ -z "$(git status --porcelain)" ] || { echo "error: commit or stash your changes first" >&2; exit 1; }
if git rev-parse -q --verify "refs/tags/v$version" >/dev/null; then
    echo "error: v$version is already tagged" >&2
    exit 1
fi

current=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)
if [ "$current" != "$version" ]; then
    # The first `version =` is the package's own.
    awk -v v="$version" '!done && /^version = "/ { $0 = "version = \"" v "\""; done = 1 } { print }' Cargo.toml > Cargo.toml.new
    mv Cargo.toml.new Cargo.toml
    cargo update --workspace --quiet
    git add Cargo.toml Cargo.lock
    git commit --quiet -m "Muxin $version"
    echo "▸ version $current → $version"
fi
git tag -a "v$version" -m "Muxin $version"
echo "▸ tagged v$version on $(git rev-parse --short HEAD)"

if [ -n "$push" ]; then
    branch=$(git branch --show-current)
    main=
    if [ -n "$branch" ] && [ "$branch" != main ]; then
        git fetch --quiet origin main
        if git merge-base --is-ancestor origin/main HEAD && git merge-base --is-ancestor main HEAD 2>/dev/null; then
            main=1
        else
            echo "note: main can't fast-forward to $branch, so it stays where it is" >&2
        fi
    fi
    if [ -n "$main" ]; then
        git push --quiet origin HEAD HEAD:main "v$version"
        git branch --force main HEAD
        echo "▸ main moved up to $branch"
    else
        git push --quiet origin HEAD "v$version"
    fi
    echo "▸ pushed: the release workflow is building it (gh run watch, or Actions on GitHub)"
else
    echo "Push it to build the release: git push origin HEAD v$version"
fi

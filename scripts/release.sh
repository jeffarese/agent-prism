#!/bin/sh
# Releases agent-prism:
#
#   sh scripts/release.sh 0.2.0
#
# From a clean main that matches origin, it sets the workspace version
# (committing and pushing the bump if it changed), tests, and pushes tag
# vX.Y.Z. The Release workflow then builds agp for macOS and Linux and
# publishes the binaries, their SHA256SUMS and build provenance on the
# GitHub release, where scripts/install.sh finds them.
set -eu
die() { echo "release: $*" >&2; exit 1; }

version=${1:-}
echo "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' || die "usage: sh scripts/release.sh X.Y.Z"
tag=v$version

cd "$(dirname "$0")/.."
for tool in git cargo perl; do
  command -v "$tool" >/dev/null || die "needs $tool"
done
[ "$(git branch --show-current)" = main ] || die "switch to main first"
[ -z "$(git status --porcelain)" ] || die "commit or stash your changes first"
git fetch -q origin main --tags
[ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] || die "main differs from origin/main"
if git rev-parse -q --verify "refs/tags/$tag" >/dev/null; then die "$tag exists already"; fi

# The version lives once, in [workspace.package].
current=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
[ -n "$current" ] || die "no version in [workspace.package] of Cargo.toml"
if [ "$current" != "$version" ]; then
  echo "release: $current -> $version"
  perl -pi -e 'if (/^\[workspace\.package\]/) { $in = 1 } elsif (/^\[/) { $in = 0 }
    if ($in && !$done && s/^version = "[^"]*"$/version = "'"$version"'"/) { $done = 1 }' Cargo.toml
  cargo update --workspace --offline -q
fi
if ! cargo test --workspace --locked -q; then
  git checkout -q -- Cargo.toml Cargo.lock
  die "tests fail; nothing was released"
fi
if [ -n "$(git status --porcelain)" ]; then
  git commit -q -m "Release $version" -- Cargo.toml Cargo.lock
  git push -q origin main
fi

git tag -a "$tag" -m "agent-prism $version"
git push -q origin "$tag"
echo "release: pushed $tag; the Release workflow builds and publishes it:"
echo "release: https://github.com/jeffarese/agent-prism/actions/workflows/release.yml"

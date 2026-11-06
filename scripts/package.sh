#!/bin/sh
# Packages a built agp the way releases ship it: dist/agp-<target>.tar.gz
# (agp, LICENSE, README.md in a folder of the same name) and its line in
# dist/SHA256SUMS.
#
#   sh scripts/package.sh <target> [<agp binary>]
#
# The binary defaults to target/<target>/dist/agp (cargo build --profile dist).
set -eu
target=${1:?usage: sh scripts/package.sh <target> [<agp binary>]}
cd "$(dirname "$0")/.."
bin=${2:-target/$target/dist/agp}
[ -x "$bin" ] || { echo "package: no $bin; build it first" >&2; exit 1; }
name=agp-$target
rm -rf "dist/$name" "dist/$name.tar.gz"
mkdir -p "dist/$name"
cp "$bin" LICENSE README.md "dist/$name/"
tar -C dist -czf "dist/$name.tar.gz" "$name"
rm -rf "dist/$name"
cd dist
if command -v sha256sum >/dev/null 2>&1; then sum=$(sha256sum "$name.tar.gz"); else sum=$(shasum -a 256 "$name.tar.gz"); fi
touch SHA256SUMS
grep -v "  $name.tar.gz\$" SHA256SUMS > SHA256SUMS.new || :
echo "$sum" >> SHA256SUMS.new
mv SHA256SUMS.new SHA256SUMS
echo "package: dist/$name.tar.gz"

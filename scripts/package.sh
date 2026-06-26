#!/usr/bin/env bash
set -euo pipefail

version="$(awk -F' = ' '$1 == "version" { gsub(/"/, "", $2); print $2; exit }' Cargo.toml)"
target="$(rustc -vV | awk '/host:/ { print $2 }')"
name="khazad-doom-${version}-${target}"
stage="dist/$name"

cargo build --release
rm -rf "$stage"
mkdir -p "$stage/bin"
install -m 0755 "target/release/khazad-doom" "$stage/bin/khazad-doom"
cp README.md "$stage/README.md"
if [[ -f LICENSE ]]; then
  cp LICENSE "$stage/LICENSE"
fi

tar -C dist -czf "dist/$name.tar.gz" "$name"
rm -rf "$stage"

echo "Wrote dist/$name.tar.gz"

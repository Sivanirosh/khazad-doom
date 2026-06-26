#!/usr/bin/env bash
set -euo pipefail

version="$(awk -F' = ' '$1 == "version" { gsub(/"/, "", $2); print $2; exit }' Cargo.toml)"
cargo_bin="${CARGO:-cargo}"
rustc_bin="${RUSTC:-rustc}"
if ! command -v "$cargo_bin" >/dev/null 2>&1 && [[ -x "$HOME/.cargo/bin/cargo" ]]; then
  cargo_bin="$HOME/.cargo/bin/cargo"
fi
if ! command -v "$rustc_bin" >/dev/null 2>&1 && [[ -x "$HOME/.cargo/bin/rustc" ]]; then
  rustc_bin="$HOME/.cargo/bin/rustc"
fi
target="$($rustc_bin -vV | awk '/host:/ { print $2 }')"
name="khazad-doom-${version}-${target}"
stage="dist/$name"

"$cargo_bin" build --release
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

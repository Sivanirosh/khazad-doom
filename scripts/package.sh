#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/.." && pwd)"
version="$(awk -F' = ' '$1 == "version" { gsub(/"/, "", $2); print $2; exit }' "$repo_root/Cargo.toml")"
cargo_bin="${CARGO:-cargo}"
rustc_bin="${RUSTC:-rustc}"
if ! command -v "$cargo_bin" >/dev/null 2>&1 && [[ -x "$HOME/.cargo/bin/cargo" ]]; then
  cargo_bin="$HOME/.cargo/bin/cargo"
fi
if ! command -v "$rustc_bin" >/dev/null 2>&1 && [[ -x "$HOME/.cargo/bin/rustc" ]]; then
  rustc_bin="$HOME/.cargo/bin/rustc"
fi
target="$("$rustc_bin" -vV | awk '/host:/ { print $2 }')"
name="khazad-doom-${version}-${target}"
dist_dir="$repo_root/dist"
stage="$dist_dir/$name"

"$cargo_bin" build --release --manifest-path "$repo_root/Cargo.toml" --target-dir "$repo_root/target"
rm -rf "$stage"
mkdir -p "$stage/bin"
install -m 0755 "$repo_root/target/release/khazad-doom" "$stage/bin/khazad-doom"
cp "$repo_root/README.md" "$stage/README.md"
if [[ -f "$repo_root/LICENSE" ]]; then
  cp "$repo_root/LICENSE" "$stage/LICENSE"
fi

tar -C "$dist_dir" -czf "$dist_dir/$name.tar.gz" "$name"
rm -rf "$stage"

echo "Wrote $dist_dir/$name.tar.gz"

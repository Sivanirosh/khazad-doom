#!/usr/bin/env bash
set -euo pipefail

prefix="${PREFIX:-$HOME/.local}"
bin_dir="$prefix/bin"
cargo_bin="${CARGO:-cargo}"
if ! command -v "$cargo_bin" >/dev/null 2>&1 && [[ -x "$HOME/.cargo/bin/cargo" ]]; then
  cargo_bin="$HOME/.cargo/bin/cargo"
fi

"$cargo_bin" build --release
mkdir -p "$bin_dir"
install -m 0755 "target/release/khazad-doom" "$bin_dir/khazad-doom"

cat <<MSG
Installed khazad-doom to $bin_dir/khazad-doom

If needed, add this to your shell profile:
  export PATH="$bin_dir:\$PATH"
MSG

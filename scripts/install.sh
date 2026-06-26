#!/usr/bin/env bash
set -euo pipefail

prefix="${PREFIX:-$HOME/.local}"
bin_dir="$prefix/bin"

cargo build --release
mkdir -p "$bin_dir"
install -m 0755 "target/release/khazad-doom" "$bin_dir/khazad-doom"

cat <<MSG
Installed khazad-doom to $bin_dir/khazad-doom

If needed, add this to your shell profile:
  export PATH="$bin_dir:\$PATH"
MSG

#!/usr/bin/env sh
set -eu
cd "$(dirname "$0")/../engine-rs"
cargo fmt --check
cargo test
cargo check --no-default-features
cargo build --release
echo "Veyra release checks passed."

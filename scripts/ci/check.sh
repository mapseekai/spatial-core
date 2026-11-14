#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
python3 scripts/ci/check-structure.py
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
RUSTDOCFLAGS="${RUSTDOCFLAGS:-} -D warnings" cargo doc --locked --no-deps
cargo run --locked --example raster_convert
cargo run --locked --example vector_convert
cargo run --locked --example raster_process
cargo run --locked --example vector_process
git diff --check

#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
: "${SPATIAL_CORE_DUCKDB_EXTENSION_DIR:?Provision the signed patched Spatial repository and official httpfs first}"
: "${SPATIAL_CORE_DUCKDB_SPATIAL_PUBLIC_KEY:?Set the trusted Spatial RSA public key file}"
test -f "$SPATIAL_CORE_DUCKDB_SPATIAL_PUBLIC_KEY"
test -f "$SPATIAL_CORE_DUCKDB_EXTENSION_DIR/httpfs.duckdb_extension"
python3 -c 'import ast,pathlib; ast.parse(pathlib.Path("scripts/native/sign-duckdb-spatial.py").read_text())'
bash -n scripts/native/build-duckdb-spatial.sh
python3 scripts/native/test-build-paths.py
python3 scripts/ci/check-structure.py
cargo clippy --locked --features duckdb --all-targets -- -D warnings
cargo run --locked --features duckdb --example duckdb_install
cargo test --locked --features duckdb
# Select only DuckDB ignored integration tests; never run PostGIS by accident.
cargo test --locked --features duckdb --lib duckdb:: -- --ignored --test-threads=1
RUSTDOCFLAGS="${RUSTDOCFLAGS:-} -D warnings" cargo doc --locked --features duckdb --no-deps
cargo run --locked --features duckdb --example duckdb_vector

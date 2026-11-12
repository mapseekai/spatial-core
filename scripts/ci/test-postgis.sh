#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
: "${SPATIAL_CORE_TEST_POSTGRES_DSN:?Set a dedicated disposable PostGIS + postgis_raster test database DSN}"
cargo test --locked --lib -- --ignored

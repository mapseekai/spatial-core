# Spatial GDAL read-error patch

This directory contains patches to DuckDB Spatial and its pinned GDAL dependency.
It restores the `Reader::Gdal` contract: a native read failure must
reach the caller and must not publish a successful partial GeoParquet file.

Pinned sources:

| Component | Revision |
| --- | --- |
| [DuckDB Spatial](https://github.com/duckdb/duckdb-spatial) | `a3dbcd1781ec26c3a1be9bd622be05b376d6f47a` |
| [DuckDB core v2.0.0](https://github.com/duckdb/duckdb) | `5766c07b594e344320d13d5ad4f4681f2737f338` |
| [vcpkg](https://github.com/microsoft/vcpkg) | `cd61e1e26a038e82d6550a3ebbe0fbbfe7da78e3` |

Spatial's pinned manifest/overlays build GDAL 3.12.4, GEOS 3.14.1 and PROJ 9.1.1.
The original `extension-ci-tools` submodule is pinned by the Spatial revision.
The explicit core revision replaces Spatial's older release-candidate gitlink so
the loadable extension matches the bundled Rust engine exactly.

`gdal-read-errors.patch` changes `src/spatial/modules/gdal/gdal_module.cpp` and
registers the GDAL patch in the existing vcpkg overlay:

- Treat nonzero Arrow return codes and CPL failures as errors, including partial
  batches and failure followed by EOF. A scoped error handler remembers failures
  even if a driver resets CPL's last-error slot; ordinary errors throw only after
  returning from GDAL. The upstream fatal-error policy is retained.
- Give each Arrow batch one shared RAII owner, including error/EOF paths.
- Propagate dataset/schema/layer initialization and metadata-open failures.
- Give `ST_Read_Meta` the same `allowed_drivers`, `open_options` and
  `sibling_files` arguments as `ST_Read`; do not force feature-count scans.
- Fill empty scan names with the binder's `C<index>` convention before applying
  DuckDB's existing Arrow name deduplication. Only geometry columns receive the
  default `geom` alias; ordinary
  `wkb_geometry` attributes keep their own meaning and values.
- Expose `spatial_core_gdal_read_contract() = 1` for Rust capability gating.

`gdal-flatgeobuf-null.patch` corrects the optimized GDAL FlatGeobuf Arrow reader.
A missing geometry gets a NULL validity bit and the previous binary offset,
instead of a non-NULL empty WKB value. The branch retains spatial filtering and
reports allocation failure. NULL and `POINT EMPTY` remain distinct.

Build with [build-duckdb-spatial.sh](../../scripts/native/build-duckdb-spatial.sh),
then run the `duckdb_install` example once to provision the stable installed path.
The signer uses DuckDB's 1 MiB segment hashes, composite SHA-256 and RSA-2048
signature footer, and verifies its signature before writing a new artifact.
The application explicitly pins the public key and DuckDB checks the signature
at load time. It never enables unsigned extensions. See
[deployment instructions](../../docs/DUCKDB.md) for the local repository layout.

The native regression tests live with their Rust owners in `src/duckdb/` and run
through [test-duckdb.sh](../../scripts/ci/test-duckdb.sh). They generate fixtures
for corrupt FGB/GPKG/Shapefile, VRT SQL failures, empty layers, open options,
remote reads and signature tampering. See [validation](../../docs/DUCKDB_VALIDATION.md)
for executed platforms and limitations. Drivers can still accept malformed data
without reporting an error; this patch is not a complete format validator.

The patch is derived from DuckDB Spatial, copyright 2018–2025 DuckDB Foundation,
under the [upstream MIT license](LICENSE). Keep this license with the patch and
redistributed native artifacts. The FlatGeobuf driver is copyright 2018–2020,
Björn Harrtell, under MIT; the pinned [GDAL license notices](GDAL-LICENSE.txt) are
included. Binary distributions must also carry the licenses
of DuckDB and the bundled GDAL/GEOS/PROJ/CURL/OpenSSL/SQLite and other dependencies;
vcpkg stages these under `vcpkg_installed/<triplet>/share/*/copyright`.

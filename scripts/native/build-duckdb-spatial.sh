#!/usr/bin/env bash
# Build and sign the pinned patch; no global installation or publication.
set -euo pipefail
if [ "$#" -ne 3 ]; then
  echo "Usage: $0 BUILD_DIRECTORY OUTPUT_DIRECTORY RSA_2048_PRIVATE_KEY" >&2
  exit 2
fi
repo=$(cd "$(dirname "$0")/../.." && pwd -P)
mkdir -p "$1" "$2"
build=$(cd "$1" && pwd -P)
output=$(cd "$2" && pwd -P)
private_key=$(python3 - "$3" "$output" <<'PY'
from pathlib import Path
import sys
key = Path(sys.argv[1]).resolve(strict=True)
output = Path(sys.argv[2])
if any(parent.samefile(output) for parent in key.parents):
    print("Keep the private key outside the deployment directory", file=sys.stderr)
    sys.exit(2)
print(key)
PY
)
case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) platform=osx_arm64; triplet=arm64-osx;;
  Linux-x86_64) platform=linux_amd64; triplet=x64-linux;;
  *) echo "Supported builds: macOS arm64 and Linux x86_64" >&2; exit 2;;
esac
spatial_revision=a3dbcd1781ec26c3a1be9bd622be05b376d6f47a
duckdb_revision=5766c07b594e344320d13d5ad4f4681f2737f338
vcpkg_revision=cd61e1e26a038e82d6550a3ebbe0fbbfe7da78e3
checkout() {
  local url=$1 directory=$2 revision=$3
  if [ ! -d "$directory/.git" ] && [ ! -f "$directory/.git" ]; then
    git clone --filter=blob:none "$url" "$directory"
  fi
  if [ "$(git -C "$directory" rev-parse HEAD)" != "$revision" ]; then
    test -z "$(git -C "$directory" status --porcelain)" || { echo "Dirty checkout: $directory" >&2; exit 1; }
    git -C "$directory" fetch --depth 1 origin "$revision"
    git -C "$directory" checkout --detach "$revision"
  fi
}
checkout https://github.com/duckdb/duckdb-spatial.git "$build/spatial" "$spatial_revision"
git -C "$build/spatial" submodule update --init --depth 1 extension-ci-tools
checkout https://github.com/duckdb/duckdb.git "$build/spatial/duckdb" "$duckdb_revision"
checkout https://github.com/microsoft/vcpkg.git "$build/vcpkg" "$vcpkg_revision"
allocator_patch="$repo/third_party/duckdb-spatial/duckdb-allocator-debug-accounting.patch"
if ! git -C "$build/spatial/duckdb" apply --reverse --check "$allocator_patch" 2>/dev/null; then
  git -C "$build/spatial/duckdb" apply --check "$allocator_patch"
  git -C "$build/spatial/duckdb" apply "$allocator_patch"
fi
git -C "$build/spatial/duckdb" diff --cached --quiet
test -z "$(git -C "$build/spatial/duckdb" ls-files --others --exclude-standard)"
cmp <(git -C "$build/spatial/duckdb" diff --no-ext-diff --full-index) "$allocator_patch"
patch="$repo/third_party/duckdb-spatial/gdal-read-errors.patch"
if ! git -C "$build/spatial" apply --reverse --check "$patch" 2>/dev/null; then
  git -C "$build/spatial" apply --check "$patch"
  git -C "$build/spatial" apply "$patch"
fi
# Only the pinned core gitlink and this exact patch may differ from upstream.
git -C "$build/spatial" diff --quiet -- . ':(exclude)duckdb' ':(exclude)src/spatial/modules/gdal/gdal_module.cpp' ':(exclude)vcpkg_ports/gdal/portfile.cmake'
git -C "$build/spatial" diff --cached --quiet
cmp <(git -C "$build/spatial" diff --no-ext-diff --full-index -- src/spatial/modules/gdal/gdal_module.cpp vcpkg_ports/gdal/portfile.cmake) "$patch"
gdal_patch="$repo/third_party/duckdb-spatial/gdal-flatgeobuf-null.patch"
gdal_overlay="$build/spatial/vcpkg_ports/gdal/spatial_core_flatgeobuf_null.patch"
if [ -e "$gdal_overlay" ]; then
  cmp "$gdal_patch" "$gdal_overlay"
else
  cp "$gdal_patch" "$gdal_overlay"
fi
for source in "$build/spatial/extension-ci-tools" "$build/vcpkg"; do
  test -z "$(git -C "$source" status --porcelain)" || { echo "Dirty build dependency: $source" >&2; exit 1; }
done
spatial_source=$(cd "$build/spatial" && pwd -P)
vcpkg_source=$(cd "$build/vcpkg" && pwd -P)
export VCPKG_DISABLE_METRICS=1 VCPKG_BINARY_SOURCES=clear CCACHE_DISABLE=1
export VCPKG_DOWNLOADS="$vcpkg_source/downloads"
mkdir -p "$VCPKG_DOWNLOADS"
export VCPKG_MAX_CONCURRENCY="${CMAKE_BUILD_PARALLEL_LEVEL:-2}"
if [ ! -x "$vcpkg_source/vcpkg" ]; then
  bash "$vcpkg_source/bootstrap-vcpkg.sh" -disableMetrics
fi
cmake -G Ninja -S "$spatial_source/duckdb" -B "$spatial_source/build/release" \
  -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_TOOLCHAIN_FILE="$vcpkg_source/scripts/buildsystems/vcpkg.cmake" \
  -DVCPKG_MANIFEST_DIR="$spatial_source" -DVCPKG_TARGET_TRIPLET="$triplet" -DVCPKG_HOST_TRIPLET="$triplet" -DVCPKG_BUILD=1 \
  -DDUCKDB_EXTENSION_CONFIGS="$spatial_source/extension_config.cmake" \
  -DDUCKDB_EXPLICIT_PLATFORM="$platform" -DOVERRIDE_GIT_DESCRIBE=v2.0.0 \
  -DEXTENSION_STATIC_BUILD=1 -DBUILD_EXTENSIONS_ONLY=OFF -DSTATICALLY_LINK_EXTENSIONS=core_functions \
  -DBUILD_UNITTESTS=OFF -DBUILD_SHELL=OFF -DENABLE_UNITTEST_CPP_TESTS=OFF -DDISABLE_UNITY=0
cmake --build "$spatial_source/build/release" --target spatial_loadable_extension --parallel "$VCPKG_MAX_CONCURRENCY"
python3 "$repo/scripts/native/sign-duckdb-spatial.py" \
  "$spatial_source/build/release/extension/spatial/spatial.duckdb_extension" \
  "$output/v2.0.0/$platform/spatial.duckdb_extension" "$private_key"
openssl pkey -in "$private_key" -pubout -out "$output/spatial-public.pem"
python3 - "$output" "$patch" "$gdal_patch" "$allocator_patch" "$spatial_revision" "$duckdb_revision" "$vcpkg_revision" "$platform" <<'PY'
import hashlib, json, pathlib, sys
output, patch, gdal_patch, allocator_patch = map(pathlib.Path, sys.argv[1:5])
spatial, duckdb, vcpkg, platform = sys.argv[5:]
binary = output / 'v2.0.0' / platform / 'spatial.duckdb_extension'
sha = lambda p: hashlib.sha256(p.read_bytes()).hexdigest()
(output / 'spatial-build.json').write_text(json.dumps(dict(
    spatial_revision=spatial, duckdb_revision=duckdb, vcpkg_revision=vcpkg,
    platform=platform, gdal_read_contract=1, patch_sha256=sha(patch),
    gdal_patch_sha256=sha(gdal_patch), allocator_contract=1,
    allocator_patch_sha256=sha(allocator_patch),
    binary_sha256=sha(binary), public_key_sha256=sha(output / 'spatial-public.pem'),
), indent=2) + '\n')
PY
echo "Signed Spatial repository: $output (provision matching official httpfs separately)"

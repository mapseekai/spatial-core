#!/usr/bin/env bash
# Verify the selected macOS GDAL prefix before bindgen and Rust compilation.
set -euo pipefail
trap 'echo "GDAL prefix/version verification failed at line $LINENO" >&2' ERR
prefix="$(cd "${1:?expected GDAL prefix}" && pwd -P)"
version="${2:?expected GDAL version}"

test "${GDAL_HOME:?}" = "$prefix"
test "${GDAL_INCLUDE_DIR:?}" = "$prefix/include"
test "${GDAL_LIB_DIR:?}" = "$prefix/lib"
test "$(command -v gdal-config)" = "$prefix/bin/gdal-config"
test "$(gdal-config --prefix)" = "$prefix"
test "$(gdal-config --version)" = "$version"
test "$(pkg-config --modversion gdal)" = "$version"
test "$(pkg-config --variable=prefix gdal)" = "$prefix"
test "$(pkg-config --variable=includedir gdal)" = "$GDAL_INCLUDE_DIR"
test "$(pkg-config --variable=libdir gdal)" = "$GDAL_LIB_DIR"
test "${GDAL_DATA:?}" = "$prefix/share/gdal"
test "${PROJ_DATA:?}" = "$prefix/share/proj"
test -d "$GDAL_DATA"
test -f "$PROJ_DATA/proj.db"
printf 'GDAL %s: prefix=%s\n' "$version" "$prefix"
pkg-config --cflags --libs gdal

probe="$(mktemp -d)"
trap 'rm -rf "$probe"' EXIT
cat > "$probe/check.c" <<'C'
#include <gdal.h>
#include <gdal_version.h>
#include <dlfcn.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

int main(int argc, char **argv) {
    if (argc != 3) return 1;
    const char *runtime = GDALVersionInfo("RELEASE_NAME");
    printf("GDAL header=%s runtime=%s\n", GDAL_RELEASE_NAME, runtime);
    if (strcmp(GDAL_RELEASE_NAME, argv[1]) || strcmp(runtime, argv[1])) return 1;

    Dl_info info;
    char library[PATH_MAX];
    if (!dladdr((const void *)&GDALVersionInfo, &info) ||
        !realpath(info.dli_fname, library)) return 1;
    printf("Loaded GDAL library=%s\n", library);
    size_t length = strlen(argv[2]);
    if (strncmp(library, argv[2], length) || library[length] != '/') return 1;
    return 0;
}
C
cc -Wall -Wextra -Werror -I "$GDAL_INCLUDE_DIR" "$probe/check.c" \
  -L "$GDAL_LIB_DIR" -Wl,-rpath,"$GDAL_LIB_DIR" -lgdal -o "$probe/check"
"$probe/check" "$version" "$prefix"

use std::{env, path::PathBuf};

fn main() {
    let version =
        env::var("DEP_GDAL_VERSION_NUMBER").expect("gdal-sys must report the GDAL build version");
    println!("cargo:rustc-env=SPATIAL_CORE_GDAL_BUILD_VERSION={version}");
    println!("cargo:rerun-if-changed=src/vector/overlay_sink.cpp");
    println!("cargo:rerun-if-env-changed=GDAL_INCLUDE_DIR");
    println!("cargo:rerun-if-env-changed=GDAL_HOME");
    let mut native = cc::Build::new();
    native
        .cpp(true)
        .std("c++17")
        .file("src/vector/overlay_sink.cpp");
    if let Some(include) = env::var_os("GDAL_INCLUDE_DIR")
        .map(PathBuf::from)
        .or_else(|| env::var_os("GDAL_HOME").map(|home| PathBuf::from(home).join("include")))
    {
        native.include(include);
    } else {
        // Match gdal-sys' target-aware discovery without linking GDAL a second time.
        let metadata = pkg_config::Config::new()
            .cargo_metadata(false)
            .probe("gdal")
            .expect("set GDAL_INCLUDE_DIR or install the GDAL development pkg-config metadata");
        native.includes(metadata.include_paths);
    }
    if native.get_compiler().is_like_msvc() {
        native.flag("/EHsc");
    }
    native.compile("spatial_core_overlay_sink");
    println!("cargo:rustc-check-cfg=cfg(gdal_3_12)");
    if version.parse::<u32>().expect("numeric GDAL version") >= 3_120_000 {
        println!("cargo:rustc-cfg=gdal_3_12");
    }
}

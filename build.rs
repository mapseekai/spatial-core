fn main() {
    let version = std::env::var("DEP_GDAL_VERSION_NUMBER")
        .expect("gdal-sys must report the GDAL build version");
    println!("cargo:rustc-env=SPATIAL_CORE_GDAL_BUILD_VERSION={version}");
}

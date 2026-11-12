//! Self-contained public API smoke test; no application files or services.
use anyhow::{Result, ensure};
use gdal::{DriverManager, raster::Buffer, spatial_ref::SpatialRef};
use spatial_core::{config::gdal::apply_gdal_options, raster::translate_to_path_with_progress};

fn main() -> Result<()> {
    apply_gdal_options(&[("GDAL_HTTP_TIMEOUT", "30")])?;
    let mut source =
        DriverManager::get_driver_by_name("MEM")?.create_with_band_type::<u8, _>("", 2, 2, 1)?;
    source.set_geo_transform(&[0.0, 0.1, 0.0, 1.0, 0.0, -0.1])?;
    source.set_spatial_ref(&SpatialRef::from_epsg(4326)?)?;
    let mut pixels = Buffer::new((2, 2), vec![1_u8, 2, 3, 4]);
    source.rasterband(1)?.write((0, 0), (2, 2), &mut pixels)?;

    let directory = tempfile::tempdir()?;
    let output = directory.path().join("converted.tif");
    let mut completed = false;
    let converted =
        translate_to_path_with_progress(&source, &output, vec!["-of", "GTiff"], |progress| {
            completed |= progress.fraction >= 1.0;
            true
        })?;
    ensure!(completed, "conversion must report completion");
    ensure!(converted.raster_size() == (2, 2), "unexpected raster size");
    ensure!(converted.spatial_ref()?.auth_code()? == 4326, "CRS changed");
    let read = converted
        .rasterband(1)?
        .read_as::<u8>((0, 0), (2, 2), (2, 2), None)?;
    ensure!(read.data() == [1, 2, 3, 4], "pixel data changed");
    println!("Converted 2 x 2 GeoTIFF: EPSG:4326, pixels and progress verified");
    Ok(())
}

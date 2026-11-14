//! Convert with the owning raster API, then reopen and inspect the result.
use anyhow::{Result, ensure};
use gdal::{Dataset, DriverManager, raster::Buffer, spatial_ref::SpatialRef};
use spatial_core::{
    operation::{Control, Progress},
    raster::Translate,
};
use std::{ops::ControlFlow, path::PathBuf};

fn main() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    let (source, output, driver) = if args.is_empty() {
        let source = directory.path().join("input.tif");
        let mut ds = DriverManager::get_driver_by_name("GTiff")?
            .create_with_band_type::<u8, _>(&source, 2, 2, 1)?;
        ds.set_geo_transform(&[0., 0.1, 0., 1., 0., -0.1])?;
        ds.set_spatial_ref(&SpatialRef::from_epsg(4326)?)?;
        ds.rasterband(1)?
            .write((0, 0), (2, 2), &mut Buffer::new((2, 2), vec![1u8, 2, 3, 4]))?;
        ds.close()?;
        (
            source,
            directory.path().join("converted.tif"),
            "GTiff".to_owned(),
        )
    } else {
        ensure!(args.len() == 3, "usage: raster_convert INPUT OUTPUT DRIVER");
        (
            PathBuf::from(&args[0]),
            PathBuf::from(&args[1]),
            args[2]
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("driver must be UTF-8"))?
                .to_owned(),
        )
    };
    let mut calls = 0usize;
    let mut progress = |_: &Progress| {
        calls += 1;
        ControlFlow::Continue(())
    };
    let report = Translate::new(&source, &output)
        .driver(driver)
        .run_with(&mut Control::default().on_progress(&mut progress))?;
    let ds = Dataset::open(&output)?;
    let size = ds.raster_size();
    ensure!(size.0 > 0 && size.1 > 0, "empty output");
    if args.is_empty() {
        ensure!(
            ds.rasterband(1)?.read_band_as::<u8>()?.data() == [1, 2, 3, 4],
            "pixel values changed"
        );
        ensure!(ds.spatial_ref()?.auth_code()? == 4326, "CRS changed");
    }
    println!(
        "Read back {} x {}, {} bands, {calls} progress calls, {} diagnostics",
        size.0,
        size.1,
        ds.raster_count(),
        report.diagnostics().len()
    );
    ds.close()?;
    Ok(())
}

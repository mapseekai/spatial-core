//! Run a raster operation, then reopen its output. No services are required.
use anyhow::{Result, ensure};
use gdal::{Dataset, DriverManager, raster::Buffer, spatial_ref::SpatialRef, vector::LayerAccess};
use spatial_core::raster::{BuildVrt, Contour, Dem, DemMode, FillNodata, Polygonize, Sieve, Warp};
use std::path::PathBuf;
fn main() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    let (operation, source, output) = if args.is_empty() {
        let source = directory.path().join("elevation.tif");
        let mut ds = DriverManager::get_driver_by_name("GTiff")?
            .create_with_band_type::<f32, _>(&source, 3, 3, 1)?;
        ds.set_geo_transform(&[0., 1., 0., 3., 0., -1.])?;
        ds.set_spatial_ref(&SpatialRef::from_epsg(3857)?)?;
        ds.rasterband(1)?.write(
            (0, 0),
            (3, 3),
            &mut Buffer::new((3, 3), vec![1f32, 2., 3., 1., 2., 3., 1., 2., 3.]),
        )?;
        ds.close()?;
        (
            "slope".to_owned(),
            source,
            directory.path().join("slope.tif"),
        )
    } else {
        ensure!(
            args.len() == 3,
            "usage: raster_process OPERATION INPUT OUTPUT (warp, slope, hillshade, fill, sieve, polygonize, contour, vrt)"
        );
        (
            args[0]
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("operation must be UTF-8"))?
                .to_owned(),
            PathBuf::from(&args[1]),
            PathBuf::from(&args[2]),
        )
    };
    let report = match operation.as_str() {
        "warp" => Warp::new([&source], &output)
            .target_srs("EPSG:3857")
            .run()?,
        "slope" => Dem::new(&source, &output, DemMode::Slope).scale(1.).run()?,
        "hillshade" => Dem::new(&source, &output, DemMode::Hillshade)
            .compute_edges(true)
            .run()?,
        "fill" => FillNodata::new(&source, &output).run()?,
        "sieve" => Sieve::new(&source, &output, 5).run()?,
        "polygonize" => Polygonize::new(&source, &output).run()?,
        "contour" => Contour::new(&source, &output).interval(10.).run()?,
        "vrt" => BuildVrt::new([&source], &output).run()?,
        _ => anyhow::bail!("unknown operation"),
    };
    let ds = Dataset::open(&output)?;
    if ds.raster_count() > 0 {
        let size = ds.raster_size();
        ensure!(size.0 > 0 && size.1 > 0, "empty raster");
        println!(
            "Read back raster {} x {}, {} bands",
            size.0,
            size.1,
            ds.raster_count()
        );
        if args.is_empty() {
            ensure!(
                (ds.rasterband(1)?.read_band_as::<f32>()?.data()[4] - 45.).abs() < 0.001,
                "incorrect slope"
            );
        }
    } else {
        let mut count = 0;
        for layer in ds.layers() {
            count += layer.feature_count();
        }
        println!("Read back {count} vector features");
    }
    ds.close()?;
    println!("{} diagnostics", report.diagnostics().len());
    Ok(())
}

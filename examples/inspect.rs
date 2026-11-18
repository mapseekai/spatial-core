use spatial_core::{
    raster::{self, HistogramOptions, SampleLocation, StatisticsMode},
    vector,
};
fn main() -> anyhow::Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.len() == 2 {
        match args[0].as_str() {
            "raster" => {
                let result = raster::Info::new(&args[1])
                    .statistics(StatisticsMode::Approximate)
                    .run()?;
                println!("{:#?}", result.value());
                let result =
                    raster::PixelInfo::new(&args[1], SampleLocation::Pixel { column: 0, row: 0 })
                        .run()?;
                println!("{:#?}", result.value());
            }
            "vector" => println!(
                "{:#?}",
                vector::Info::new(&args[1])
                    .feature_count(vector::ScanMode::Exact)
                    .extent(vector::ScanMode::Exact)
                    .run()?
                    .value()
            ),
            _ => anyhow::bail!("usage: inspect [raster|vector INPUT]"),
        }
        return Ok(());
    }
    anyhow::ensure!(args.is_empty(), "usage: inspect [raster|vector INPUT]");
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("pixels.tif");
    let mut ds = gdal::DriverManager::get_driver_by_name("GTiff")?
        .create_with_band_type::<u8, _>(&path, 2, 2, 1)?;
    ds.set_geo_transform(&[0., 1., 0., 2., 0., -1.])?;
    ds.rasterband(1)?.write(
        (0, 0),
        (2, 2),
        &mut gdal::raster::Buffer::new((2, 2), vec![1, 2, 3, 4]),
    )?;
    ds.close()?;
    let result = raster::Info::new(&path)
        .statistics(StatisticsMode::Exact)
        .histogram(HistogramOptions::new(0.5, 4.5, 4))
        .run()?;
    assert_eq!(
        result.value().bands[0].histogram.as_ref().unwrap().counts,
        [1, 1, 1, 1]
    );
    let result =
        raster::PixelInfo::new(&path, SampleLocation::Pixel { column: 1, row: 1 }).run()?;
    assert!(result.value().bands[0].valid);
    println!("metadata, exact statistics, histogram and pixel query verified");
    Ok(())
}

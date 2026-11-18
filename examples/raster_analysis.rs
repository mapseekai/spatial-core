use spatial_core::raster::{
    self, BuildOverviews, Calculate, ClearOverviews, ExpressionDialect, HistogramOptions,
    OverviewStorage, Reclassify, RefreshOverviews, Resampling, StatisticsMode, Unmatched,
};
use std::{path::PathBuf, time::Instant};
fn main() -> anyhow::Result<()> {
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    anyhow::ensure!(args.len() <= 1, "usage: raster_analysis [INPUT]");
    let dir = tempfile::tempdir()?;
    let source = if let Some(path) = args.first() {
        PathBuf::from(path)
    } else {
        let path = dir.path().join("source.tif");
        let mut ds = gdal::DriverManager::get_driver_by_name("GTiff")?
            .create_with_band_type::<u8, _>(&path, 64, 64, 1)?;
        ds.set_spatial_ref(&gdal::spatial_ref::SpatialRef::from_epsg(3857)?)?;
        ds.set_geo_transform(&[0., 1., 0., 64., 0., -1.])?;
        ds.rasterband(1)?.write(
            (0, 0),
            (64, 64),
            &mut gdal::raster::Buffer::new((64, 64), (0..4096).map(|v| (v % 251) as u8).collect()),
        )?;
        ds.close()?;
        path
    };
    let start = Instant::now();
    let info = raster::Info::new(&source).run()?;
    let (width, height) = info.value().size;
    anyhow::ensure!(width > 0 && height > 0, "empty raster");
    let sample = dir.path().join("sample.tif");
    let _ = raster::Translate::new(&source, &sample)
        .bands([1])
        .source_window(0, 0, width.min(256) as u32, height.min(256) as u32)
        .run()?;
    let stats = raster::Info::new(&sample)
        .statistics(StatisticsMode::Exact)
        .run()?;
    let stats = stats.value().bands[0].statistics.as_ref().unwrap();
    let histogram = raster::Info::new(&sample)
        .histogram(HistogramOptions::new(
            stats.minimum - 0.5,
            stats.maximum + 0.5,
            16,
        ))
        .run()?;
    anyhow::ensure!(
        histogram.value().bands[0]
            .histogram
            .as_ref()
            .unwrap()
            .counts
            .iter()
            .sum::<u64>()
            > 0,
        "empty histogram"
    );
    let _ = BuildOverviews::new(&sample)
        .levels([2, 4])
        .resampling(Resampling::Average)
        .run()?;
    let _ = RefreshOverviews::new(&sample).run()?;
    let _ = ClearOverviews::new(&sample).run()?;
    let _ = BuildOverviews::new(&sample)
        .storage(OverviewStorage::Internal)
        .levels([2])
        .run()?;
    if gdal::version::version_info("VERSION_NUM").parse::<u32>()? < 3_120_000 {
        println!(
            "query and overview checks passed; calculation/classification require GDAL 3.12 and were not run"
        );
        return Ok(());
    }
    let calculated = dir.path().join("calculated.tif");
    let _ = Calculate::new(&calculated)
        .input("A", &sample)
        .dialect(ExpressionDialect::Builtin)
        .expression("sum(k=1)")
        .run()?;
    let input = gdal::Dataset::open(&sample)?;
    let output = gdal::Dataset::open(&calculated)?;
    let input_values = input.rasterband(1)?.read_band_as::<f64>()?;
    let actual = output.rasterband(1)?.read_band_as::<f64>()?;
    let calculated_nodata = output.rasterband(1)?.no_data_value();
    let mask = input
        .rasterband(1)?
        .open_mask_band()?
        .read_band_as::<u8>()?;
    for ((a, b), valid) in input_values
        .data()
        .iter()
        .zip(actual.data())
        .zip(mask.data())
    {
        if *valid != 0 {
            anyhow::ensure!((*a + 1. - *b).abs() < 1e-9, "calculation pixel mismatch");
        } else {
            anyhow::ensure!(
                calculated_nodata.is_some_and(|v| v == *b || v.is_nan() && b.is_nan()),
                "calculation lost NoData validity"
            );
        }
    }
    input.close()?;
    output.close()?;
    let classes = dir.path().join("classes.tif");
    let _ = Reclassify::new(&sample, &classes)
        .range(..=stats.mean, 1)
        .range(
            (
                std::ops::Bound::Excluded(stats.mean),
                std::ops::Bound::Unbounded,
            ),
            2,
        )
        .unmatched(Unmatched::NoData)
        .output_nodata(255)
        .data_type(raster::DataType::UInt8)
        .run()?;
    let result = gdal::Dataset::open(&classes)?;
    let values = result.rasterband(1)?.read_band_as::<u8>()?;
    for ((actual, value), valid) in values
        .data()
        .iter()
        .zip(input_values.data())
        .zip(mask.data())
    {
        let expected = if *valid == 0 {
            255
        } else if *value <= stats.mean {
            1
        } else {
            2
        };
        anyhow::ensure!(*actual == expected, "classification pixel mismatch");
    }
    anyhow::ensure!(
        result.rasterband(1)?.no_data_value() == Some(255.),
        "classification NoData metadata mismatch"
    );
    result.close()?;
    println!(
        "{}: driver={} size={}x{}, window={}x{}, statistics/histogram, overview lifecycle, builtin calculation and classification verified in {:.3}s",
        source.display(),
        info.value().driver,
        width,
        height,
        width.min(256),
        height.min(256),
        start.elapsed().as_secs_f64()
    );
    Ok(())
}

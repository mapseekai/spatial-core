use gdal::vector::{
    Feature, FieldDefn, Geometry, LayerAccess, LayerOptions, OGRFieldType, OGRwkbGeometryType,
};
use spatial_core::vector::{
    self, CheckCoverage, CheckGeometry, CleanCoverage, CoverageLimits, LayerSource, Overlay,
    OverlayOperation, SimplifyCoverage,
};
use std::{path::PathBuf, time::Instant};
fn main() -> anyhow::Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    anyhow::ensure!(args.len() <= 2, "usage: vector_analysis [INPUT [LAYER]]");
    let dir = tempfile::tempdir()?;
    let start = Instant::now();
    let input = if let Some(path) = args.first() {
        PathBuf::from(path)
    } else {
        let path = dir.path().join("parcels.gpkg");
        let mut ds = gdal::DriverManager::get_driver_by_name("GPKG")?.create_vector_only(&path)?;
        let srs = gdal::spatial_ref::SpatialRef::from_epsg(3857)?;
        let layer = ds.create_layer(LayerOptions {
            name: "parcels",
            srs: Some(&srs),
            ty: OGRwkbGeometryType::wkbPolygon,
            ..Default::default()
        })?;
        FieldDefn::new("parcel_id", OGRFieldType::OFTInteger)?.add_to_layer(&layer)?;
        for (i, wkt) in [
            "POLYGON ((0 0,2 0,2 2,0 2,0 0))",
            "POLYGON ((2 0,4 0,4 2,2 2,2 0))",
        ]
        .iter()
        .enumerate()
        {
            let mut feature = Feature::new(layer.defn())?;
            feature.set_field_integer(0, i as i32)?;
            feature.set_geometry(Geometry::from_wkt(wkt)?)?;
            feature.create(&layer)?;
        }
        ds.close()?;
        path
    };
    let schema = vector::Info::new(&input)
        .feature_count(vector::ScanMode::Exact)
        .extent(vector::ScanMode::Exact)
        .run()?;
    let layer = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| schema.value().layers[0].name.clone());
    let source = LayerSource::new(&input).layer(&layer);
    let check = CheckGeometry::new(source.clone()).sample_limit(10).run()?;
    println!(
        "{} layer={layer}: checked={}, invalid={}, null={}, empty={}",
        input.display(),
        check.value().checked,
        check.value().invalid,
        check.value().null,
        check.value().empty
    );
    if !check.value().is_valid() {
        println!("invalid source geometries reported; repair explicitly before overlay/coverage");
        return Ok(());
    }
    let overlay = dir.path().join("intersection.gpkg");
    let _ = Overlay::new(source.clone(), source.clone(), &overlay)
        .operation(OverlayOperation::Intersection)
        .run()?;
    let checked_overlay = CheckGeometry::new(&overlay).run()?;
    anyhow::ensure!(
        checked_overlay.value().is_valid(),
        "overlay output has invalid geometry"
    );
    anyhow::ensure!(
        check.value().checked == 0 || checked_overlay.value().checked > 0,
        "self intersection unexpectedly lost every feature"
    );
    if gdal::version::version_info("VERSION_NUM").parse::<u32>()? < 3_120_000 {
        println!("geometry and overlay checks passed; coverage requires GDAL 3.12 and was not run");
        return Ok(());
    }
    let limits = CoverageLimits {
        max_features: Some(100_000),
        max_vertices: Some(5_000_000),
    };
    let coverage = CheckCoverage::new(source.clone())
        .limits(limits)
        .sample_limit(10)
        .run()?;
    println!(
        "coverage: checked={}, invalid_geometries={}, invalid_edges={}",
        coverage.value().checked,
        coverage.value().invalid_geometries,
        coverage.value().invalid_edges
    );
    let clean = dir.path().join("cleaned.gpkg");
    match CleanCoverage::new(source, &clean).limits(limits).run() {
        Ok(_) => (),
        Err(error) if error.kind() == spatial_core::operation::ErrorKind::Unsupported => {
            println!(
                "overlay and coverage checks completed; cleaning is unavailable in this build: {error}"
            );
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    }
    let cleaned = CheckCoverage::new(&clean).run()?;
    anyhow::ensure!(
        cleaned.value().is_valid(),
        "cleaned output has invalid coverage"
    );
    let simple = dir.path().join("simplified.gpkg");
    let _ = SimplifyCoverage::new(&clean, &simple)
        .tolerance(0.)
        .preserve_boundary(true)
        .limits(limits)
        .run()?;
    anyhow::ensure!(
        CheckCoverage::new(&simple).run()?.value().is_valid(),
        "simplified output has invalid coverage"
    );
    println!(
        "overlay/coverage/clean/simplify outputs read back in {:.3}s",
        start.elapsed().as_secs_f64()
    );
    Ok(())
}

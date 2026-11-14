//! Geometry processing uses the same conversion builder and resource contract.
use anyhow::{Result, ensure};
use gdal::{Dataset, vector::LayerAccess};
use spatial_core::vector::{Clip, Translate};
fn main() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let source = directory.path().join("roads.geojson");
    let output = directory.path().join("clipped.gpkg");
    std::fs::write(
        &source,
        r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"name":"road"},"geometry":{"type":"LineString","coordinates":[[0,0],[1,0.1],[2,0]]}}]}"#,
    )?;
    let _ = Translate::new(&source, &output)
        .clip_source(Clip::Bounds([0.5, -1., 1.5, 1.]))
        .simplify(0.2)
        .run()?;
    let ds = Dataset::open(output)?;
    {
        let mut layer = ds.layer(0)?;
        ensure!(layer.feature_count() == 1, "unexpected feature count");
        let feature = layer.features().next().unwrap();
        let geometry = feature.geometry().unwrap();
        ensure!(geometry.point_count() == 2, "not simplified");
        let extent = geometry.envelope();
        ensure!(
            (extent.MinX - 0.5).abs() < 1e-9 && (extent.MaxX - 1.5).abs() < 1e-9,
            "not clipped"
        );
    }
    ds.close()?;
    println!("Read back one clipped and simplified road");
    Ok(())
}

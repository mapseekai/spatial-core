//! With no arguments, generate a fixture and verify its GPKG conversion.
//! Or: cargo run --example vector_convert -- INPUT OUTPUT DRIVER [LAYER]
use gdal::{Dataset, vector::LayerAccess};
use spatial_core::vector::Translate;
use std::{ffi::OsString, path::PathBuf, time::Instant};

fn main() -> anyhow::Result<()> {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let directory = tempfile::tempdir()?;
    let (input, output, driver, layer) = if args.is_empty() {
        let input = directory.path().join("points.geojson");
        std::fs::write(
            &input,
            r#"{"type":"FeatureCollection","name":"points","features":[{"type":"Feature","properties":{"name":"示例","value":7},"geometry":{"type":"Point","coordinates":[120,30]}}]}"#,
        )?;
        (
            input,
            directory.path().join("points.gpkg"),
            "GPKG".to_owned(),
            None,
        )
    } else if matches!(args.len(), 3 | 4) {
        let driver = args[2]
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("driver must be UTF-8"))?
            .to_owned();
        let layer = args
            .get(3)
            .map(|name| {
                name.to_str()
                    .map(str::to_owned)
                    .ok_or_else(|| anyhow::anyhow!("layer must be UTF-8"))
            })
            .transpose()?;
        (
            PathBuf::from(&args[0]),
            PathBuf::from(&args[1]),
            driver,
            layer,
        )
    } else {
        anyhow::bail!("usage: vector_convert [INPUT OUTPUT DRIVER [LAYER]]");
    };
    let started = Instant::now();
    let mut job = Translate::new(&input, &output).driver(&driver);
    if let Some(name) = &layer {
        job = job.layers([name.clone()]);
    }
    let report = job.run()?;
    let source = Dataset::open(&input)?;
    let expected: u64 = source
        .layers()
        .filter(|l| layer.as_ref().is_none_or(|name| l.name() == *name))
        .map(|l| l.feature_count())
        .sum();
    if driver.eq_ignore_ascii_case("PGDUMP") {
        anyhow::ensure!(std::fs::metadata(&output)?.len() > 0, "empty SQL dump");
        println!(
            "exported SQL for {expected} features; {} diagnostics; {:?}",
            report.diagnostics().len(),
            started.elapsed()
        );
    } else {
        let result = Dataset::open(&output)?;
        let actual: u64 = result.layers().map(|l| l.feature_count()).sum();
        anyhow::ensure!(
            actual == expected,
            "feature count changed: {expected} -> {actual}"
        );
        println!(
            "{driver}: {actual} features in {} layers; {} diagnostics; {:?}",
            result.layer_count(),
            report.diagnostics().len(),
            started.elapsed()
        );
        result.close()?;
    }
    source.close()?;
    Ok(())
}

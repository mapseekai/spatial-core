use gdal::{
    Dataset, DriverManager,
    spatial_ref::SpatialRef,
    vector::{
        Feature, FieldDefn, Geometry, LayerAccess, LayerOptions, OGRFieldType, OGRwkbGeometryType,
    },
};
use std::path::Path;
pub(super) fn polygons(path: &Path, wkts: &[&str]) -> anyhow::Result<()> {
    let mut ds = DriverManager::get_driver_by_name("GPKG")?.create_vector_only(path)?;
    let srs = SpatialRef::from_epsg(3857)?;
    let layer = ds.create_layer(LayerOptions {
        name: "parcels",
        srs: Some(&srs),
        ty: OGRwkbGeometryType::wkbPolygon,
        ..Default::default()
    })?;
    FieldDefn::new("id", OGRFieldType::OFTInteger)?.add_to_layer(&layer)?;
    for (i, wkt) in wkts.iter().enumerate() {
        let mut feature = Feature::new(layer.defn())?;
        feature.set_field_integer(0, i as i32 + 10)?;
        if !wkt.is_empty() {
            feature.set_geometry(Geometry::from_wkt(wkt)?)?;
        }
        feature.create(&layer)?;
    }
    ds.close()?;
    Ok(())
}
pub(super) fn area(path: &Path) -> anyhow::Result<f64> {
    let ds = Dataset::open(path)?;
    let mut layer = ds.layer(0)?;
    let area = layer
        .features()
        .filter_map(|f| f.geometry().map(Geometry::area))
        .sum();
    ds.close()?;
    Ok(area)
}

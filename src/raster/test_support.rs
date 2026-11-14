use gdal::{
    Dataset, DriverManager,
    raster::{Buffer, GdalType},
    spatial_ref::SpatialRef,
};
use std::path::Path;

pub(super) fn fixture<T: GdalType + Copy>(
    path: &Path,
    width: usize,
    height: usize,
    values: &[T],
) -> anyhow::Result<()> {
    let mut ds = DriverManager::get_driver_by_name("GTiff")?
        .create_with_band_type::<T, _>(path, width, height, 1)?;
    ds.set_spatial_ref(&SpatialRef::from_epsg(4326)?)?;
    ds.set_geo_transform(&[0., 1., 0., height as f64, 0., -1.])?;
    ds.rasterband(1)?.write(
        (0, 0),
        (width, height),
        &mut Buffer::new((width, height), values.to_vec()),
    )?;
    ds.close()?;
    Ok(())
}
pub(super) fn pixels(path: &Path) -> anyhow::Result<Vec<f64>> {
    let ds = Dataset::open(path)?;
    let data = ds.rasterband(1)?.read_band_as::<f64>()?.data().to_vec();
    ds.close()?;
    Ok(data)
}

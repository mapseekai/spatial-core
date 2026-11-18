use super::{DataType, PixelValue, options, value};
use crate::{
    config::gdal::GdalOptions,
    dataset::{Kind, Source},
    gdal_ext::{
        execution::close,
        ffi::{checked_gdal_call, last_null_pointer_err},
        query::{self, inspect_error},
    },
    operation::{Control, Error, Outcome, Result, Stage},
};
use gdal::{Dataset, DriverManager, Metadata, cpl::CslStringList, raster::RasterBand};
use std::collections::BTreeMap;

/// How band statistics are obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatisticsMode {
    /// Never force scanning; unavailable statistics remain None.
    CachedOnly,
    /// Permit GDAL's overview or block sampling.
    Approximate,
    /// Scan pixels rather than trusting cached statistics.
    Exact,
}
/// Provenance of returned statistics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatisticsOrigin {
    /// Existing metadata; approximation is only known when recorded by the driver.
    Cached { approximate: Option<bool> },
    /// Newly computed, possibly using samples.
    Computed { approximate: bool },
}
/// Numeric band statistics in stored pixel units.
#[derive(Debug, Clone, PartialEq)]
pub struct BandStatistics {
    /// Minimum valid value.
    pub minimum: f64,
    /// Maximum valid value.
    pub maximum: f64,
    /// Arithmetic mean.
    pub mean: f64,
    /// Population standard deviation.
    pub standard_deviation: f64,
    /// Cache/computation and approximation information.
    pub origin: StatisticsOrigin,
}
/// Fixed histogram bins; the upper boundary is exclusive.
#[derive(Debug, Clone)]
pub struct HistogramOptions {
    minimum: f64,
    maximum: f64,
    buckets: usize,
    include_out_of_range: bool,
    approximate: bool,
}
impl HistogramOptions {
    /// Describe a finite range with 1..=1,000,000 bins (at most 8 MB of counts).
    pub fn new(minimum: f64, maximum: f64, buckets: usize) -> Self {
        Self {
            minimum,
            maximum,
            buckets,
            include_out_of_range: false,
            approximate: false,
        }
    }
    /// Count values outside the range in the nearest end bin.
    pub fn include_out_of_range(mut self, value: bool) -> Self {
        self.include_out_of_range = value;
        self
    }
    /// Allow native sampling instead of reading all pixels.
    pub fn approximate(mut self, value: bool) -> Self {
        self.approximate = value;
        self
    }
    fn validate(&self) -> Result<()> {
        if !self.minimum.is_finite()
            || !self.maximum.is_finite()
            || self.minimum >= self.maximum
            || !(1..=1_000_000).contains(&self.buckets)
        {
            return Err(Error::invalid(
                "histogram requires a finite increasing range and 1..=1000000 buckets",
            ));
        }
        Ok(())
    }
}
/// Owned histogram results.
#[derive(Debug, Clone)]
pub struct Histogram {
    /// Lower boundary.
    pub minimum: f64,
    /// Exclusive upper boundary.
    pub maximum: f64,
    /// Counts in ascending bins.
    pub counts: Vec<u64>,
    /// Whether native sampling was permitted.
    pub approximate: bool,
}
/// One band's metadata and explicitly requested analysis.
#[derive(Debug)]
pub struct BandInfo {
    /// One-based index.
    pub index: usize,
    /// Native pixel type.
    pub data_type: DataType,
    /// Native block size in pixels.
    pub block_size: (usize, usize),
    /// Exact NoData representation when the driver supplies one.
    pub nodata: Option<PixelValue>,
    /// Physical scale; None means unspecified.
    pub scale: Option<f64>,
    /// Physical offset; None means unspecified.
    pub offset: Option<f64>,
    /// GDAL's color interpretation name.
    pub color_interpretation: String,
    /// Native GDAL mask flags.
    pub mask_flags: i32,
    /// Overview dimensions, in the driver's order.
    pub overviews: Vec<(usize, usize)>,
    /// Requested statistics, if available.
    pub statistics: Option<BandStatistics>,
    /// Requested histogram.
    pub histogram: Option<Histogram>,
}
/// Owned raster metadata. No native handle escapes the operation.
#[derive(Debug)]
pub struct RasterInfo {
    /// Installed driver's canonical name.
    pub driver: String,
    /// Base width and height.
    pub size: (usize, usize),
    /// Spatial reference WKT, if present.
    pub spatial_ref: Option<String>,
    /// Six affine coefficients, if present.
    pub geo_transform: Option<[f64; 6]>,
    /// Number of ground control points.
    pub gcp_count: usize,
    /// Whether RPC metadata exists.
    pub has_rpc: bool,
    /// Band results in selection order.
    pub bands: Vec<BandInfo>,
    /// Explicitly requested metadata domains. Entries retain native KEY=VALUE text.
    pub metadata: BTreeMap<String, Vec<String>>,
    /// Subdataset descriptors; these datasets are not opened.
    pub subdatasets: Vec<String>,
}
/// Inspect metadata without implicitly computing statistics or opening subdatasets.
#[derive(Debug, Clone)]
#[must_use]
pub struct Info {
    source: Source,
    bands: Option<Vec<usize>>,
    statistics: Option<StatisticsMode>,
    histogram: Option<HistogramOptions>,
    domains: Vec<String>,
    config: GdalOptions,
}
impl Info {
    /// Describe an inspection; construction performs no I/O.
    pub fn new(source: impl Into<Source>) -> Self {
        Self {
            source: source.into(),
            bands: None,
            statistics: None,
            histogram: None,
            domains: Vec::new(),
            config: GdalOptions::default(),
        }
    }
    /// Select one-based bands; omission inspects all bands.
    pub fn bands(mut self, bands: impl IntoIterator<Item = usize>) -> Self {
        self.bands = Some(bands.into_iter().collect());
        self
    }
    /// Request statistics; the default is metadata only.
    pub fn statistics(mut self, mode: StatisticsMode) -> Self {
        self.statistics = Some(mode);
        self
    }
    /// Request a fixed-range histogram for each selected band.
    pub fn histogram(mut self, options: HistogramOptions) -> Self {
        self.histogram = Some(options);
        self
    }
    /// Read this metadata domain without interpreting its values.
    pub fn metadata_domain(mut self, domain: impl Into<String>) -> Self {
        self.domains.push(domain.into());
        self
    }
    /// Scope native configuration through input close.
    pub fn gdal_options(mut self, config: GdalOptions) -> Self {
        self.config = config;
        self
    }
    /// Inspect synchronously and close all native resources.
    pub fn run(self) -> Result<Outcome<RasterInfo>> {
        self.run_with(&mut Control::default())
    }
    /// Inspect with borrowed progress and cancellation.
    pub fn run_with(self, control: &mut Control<'_>) -> Result<Outcome<RasterInfo>> {
        if let Some(bands) = &self.bands {
            for &index in bands {
                options::band(index)?;
            }
        }
        if let Some(histogram) = &self.histogram {
            histogram.validate()?;
        }
        if self.domains.iter().any(|d| d.contains('\0')) {
            return Err(Error::invalid("metadata domain contains NUL"));
        }
        query::run(
            self.source,
            Kind::Raster,
            self.config,
            control,
            |input, progress| {
                let bands = self
                    .bands
                    .unwrap_or_else(|| (1..=input.raster_count()).collect());
                for &index in &bands {
                    options::source_band(input, index)?;
                }
                let computed = self.histogram.is_some()
                    || matches!(
                        self.statistics,
                        Some(StatisticsMode::Exact | StatisticsMode::Approximate)
                    );
                let view = if computed && !bands.is_empty() {
                    Some(statistics_view(input, &bands)?)
                } else {
                    None
                };
                let result = (|| {
                    let mut result = Vec::with_capacity(bands.len());
                    for (position, &index) in bands.iter().enumerate() {
                        query::pulse(
                            progress,
                            position as f64 / bands.len().max(1) as f64,
                            c"inspecting raster",
                        )?;
                        let band = options::source_band(input, index)?;
                        let overview_count = band.overview_count().map_err(inspect_error)?;
                        let overviews = (0..overview_count)
                            .map(|i| {
                                band.overview(i as usize)
                                    .map(|b| b.size())
                                    .map_err(inspect_error)
                            })
                            .collect::<Result<Vec<_>>>()?;
                        let statistics = match self.statistics {
                            None => None,
                            Some(StatisticsMode::CachedOnly) => band
                                .get_statistics(false, true)
                                .map_err(inspect_error)?
                                .map(|s| BandStatistics {
                                    minimum: s.min,
                                    maximum: s.max,
                                    mean: s.mean,
                                    standard_deviation: s.std_dev,
                                    origin: StatisticsOrigin::Cached {
                                        approximate: band
                                            .metadata_item("STATISTICS_APPROXIMATE", "")
                                            .map(|s| s.eq_ignore_ascii_case("YES")),
                                    },
                                }),
                            Some(mode) => {
                                let virtual_band = view
                                    .as_ref()
                                    .expect("statistics view")
                                    .rasterband(position + 1)
                                    .map_err(inspect_error)?;
                                let mut values = [0.0; 4];
                                // SAFETY: the view band, output scalars and callback state are live for this call.
                                let status = checked_gdal_call(|| unsafe {
                                    gdal_sys::GDALComputeRasterStatistics(
                                        virtual_band.c_rasterband(),
                                        i32::from(mode == StatisticsMode::Approximate),
                                        &mut values[0],
                                        &mut values[1],
                                        &mut values[2],
                                        &mut values[3],
                                        progress.native_callback(),
                                        progress.user_data(),
                                    )
                                })
                                .map_err(inspect_error)?;
                                query::check_progress(progress)?;
                                if status != gdal_sys::CPLErr::CE_None {
                                    return Err(inspect_error(last_null_pointer_err(
                                        "GDALComputeRasterStatistics",
                                    )));
                                }
                                Some(BandStatistics {
                                    minimum: values[0],
                                    maximum: values[1],
                                    mean: values[2],
                                    standard_deviation: values[3],
                                    origin: StatisticsOrigin::Computed {
                                        approximate: mode == StatisticsMode::Approximate,
                                    },
                                })
                            }
                        };
                        let histogram = if let Some(options) = &self.histogram {
                            let virtual_band = view
                                .as_ref()
                                .expect("histogram view")
                                .rasterband(position + 1)
                                .map_err(inspect_error)?;
                            let mut counts = vec![0u64; options.buckets];
                            // SAFETY: count capacity matches the validated bucket count; all handles are live.
                            let status = checked_gdal_call(|| unsafe {
                                gdal_sys::GDALGetRasterHistogramEx(
                                    virtual_band.c_rasterband(),
                                    options.minimum,
                                    options.maximum,
                                    options.buckets as i32,
                                    counts.as_mut_ptr(),
                                    i32::from(options.include_out_of_range),
                                    i32::from(options.approximate),
                                    progress.native_callback(),
                                    progress.user_data(),
                                )
                            })
                            .map_err(inspect_error)?;
                            query::check_progress(progress)?;
                            if status != gdal_sys::CPLErr::CE_None {
                                return Err(inspect_error(last_null_pointer_err(
                                    "GDALGetRasterHistogramEx",
                                )));
                            }
                            Some(Histogram {
                                minimum: options.minimum,
                                maximum: options.maximum,
                                counts,
                                approximate: options.approximate,
                            })
                        } else {
                            None
                        };
                        // SAFETY: the borrowed band is live and GDAL owns its mask.
                        let mask_flags = unsafe { gdal_sys::GDALGetMaskFlags(band.c_rasterband()) };
                        result.push(BandInfo {
                            index,
                            data_type: band.band_type(),
                            block_size: band.block_size(),
                            nodata: value::nodata(&band),
                            scale: band.scale(),
                            offset: band.offset(),
                            color_interpretation: band.color_interpretation().name(),
                            mask_flags,
                            overviews,
                            statistics,
                            histogram,
                        });
                    }
                    let mut metadata = BTreeMap::new();
                    for domain in self.domains {
                        metadata.insert(
                            domain.clone(),
                            input.metadata_domain(&domain).unwrap_or_default(),
                        );
                    }
                    query::pulse(progress, 1.0, c"raster inspection complete")?;
                    Ok(RasterInfo {
                        driver: input.driver().short_name(),
                        size: input.raster_size(),
                        spatial_ref: input
                            .spatial_ref()
                            .ok()
                            .map(|s| s.to_wkt())
                            .transpose()
                            .map_err(inspect_error)?,
                        geo_transform: input.geo_transform().ok(),
                        gcp_count: input.gcps().len(),
                        has_rpc: input.metadata_domain("RPC").is_some_and(|v| !v.is_empty()),
                        bands: result,
                        metadata,
                        subdatasets: input.metadata_domain("SUBDATASETS").unwrap_or_default(),
                    })
                })();
                let (value, mut failure) = match result {
                    Ok(value) => (Some(value), None),
                    Err(e) => (None, Some(e)),
                };
                close(view, Stage::CloseSource, &mut failure);
                failure.map_or_else(|| Ok(value.expect("successful inspection")), Err)
            },
        )
    }
}
// Use one live source per derived band: GDAL sees the original composition and cannot
// delegate statistics or cache writes to the inspected dataset.
fn statistics_view(input: &Dataset, bands: &[usize]) -> Result<Dataset> {
    let (width, height) = input.raster_size();
    let view = DriverManager::get_driver_by_name("VRT")
        .and_then(|driver| driver.create_with_band_type::<u8, _>("", width, height, 0))
        .map_err(inspect_error)?;
    let options: CslStringList = ["subClass=VRTDerivedRasterBand", "PixelFunctionType=real"]
        .into_iter()
        .collect();
    for (position, &index) in bands.iter().enumerate() {
        let source = options::source_band(input, index)?;
        // The real pixel function can change wide-integer NoData membership.
        if matches!(source.band_type(), DataType::Int64 | DataType::UInt64) {
            return Err(Error::unsupported(
                "computed statistics/histograms on 64-bit integer bands require an explicit conversion; metadata, cached statistics and PixelInfo remain available",
            ));
        }
        // SAFETY: source, dataset, and null-terminated options are live through this call.
        let status = checked_gdal_call(|| unsafe {
            gdal_sys::GDALAddBand(
                view.c_dataset(),
                gdal_sys::GDALGetRasterDataType(source.c_rasterband()),
                options.as_ptr(),
            )
        })
        .map_err(inspect_error)?;
        if status != gdal_sys::CPLErr::CE_None {
            return Err(inspect_error(last_null_pointer_err("GDALAddBand")));
        }
        let mut dest = view.rasterband(position + 1).map_err(inspect_error)?;
        add_whole_source(&dest, &source)?;
        if source.band_type() == DataType::UInt8
            && source
                .metadata_domain("IMAGE_STRUCTURE")
                .is_some_and(|entries| {
                    entries
                        .iter()
                        .any(|s| s.eq_ignore_ascii_case("PIXELTYPE=SIGNEDBYTE"))
                })
        {
            dest.set_metadata_item("PIXELTYPE", "SIGNEDBYTE", "IMAGE_STRUCTURE")
                .map_err(inspect_error)?;
        }
        if let Some(value) = source.no_data_value() {
            dest.set_no_data_value(Some(value)).map_err(inspect_error)?;
        }
        let flags = source.mask_flags().map_err(inspect_error)?;
        let intrinsic_nodata = flags.is_nodata() && !flags.is_per_dataset();
        if !flags.is_all_valid()
            && !intrinsic_nodata
            && source.color_interpretation() != gdal::raster::ColorInterpretation::AlphaBand
        {
            dest.create_mask_band(false).map_err(inspect_error)?;
            add_whole_source(
                &dest.open_mask_band().map_err(inspect_error)?,
                &source.open_mask_band().map_err(inspect_error)?,
            )?;
        }
    }
    Ok(view)
}

fn add_whole_source(dest: &RasterBand<'_>, source: &RasterBand<'_>) -> Result<()> {
    // Public gdal_vrt.h API; gdal-sys only generates bindings for gdal.h.
    unsafe extern "system" {
        fn VRTAddSimpleSource(
            dest: gdal_sys::GDALRasterBandH,
            source: gdal_sys::GDALRasterBandH,
            src_x: i32,
            src_y: i32,
            src_width: i32,
            src_height: i32,
            dst_x: i32,
            dst_y: i32,
            dst_width: i32,
            dst_height: i32,
            resampling: *const std::ffi::c_char,
            nodata: f64,
        ) -> gdal_sys::CPLErr::Type;
    }
    let (w, h) = source.size();
    // SAFETY: dest is a VRT sourced/derived band, the input outlives the view, and
    // native band dimensions already fit GDAL's int ABI. -1234.56 is VRT_NODATA_UNSET.
    let status = checked_gdal_call(|| unsafe {
        VRTAddSimpleSource(
            dest.c_rasterband(),
            source.c_rasterband(),
            0,
            0,
            w as i32,
            h as i32,
            0,
            0,
            w as i32,
            h as i32,
            std::ptr::null(),
            -1234.56,
        )
    })
    .map_err(inspect_error)?;
    if status != gdal_sys::CPLErr::CE_None {
        return Err(inspect_error(last_null_pointer_err("VRTAddSimpleSource")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn statistics_read_composed_sources_in_selected_order_without_writes() -> anyhow::Result<()> {
        use super::super::test_support::fixture;
        let dir = tempfile::tempdir()?;
        let first = dir.path().join("left.tif");
        let second = dir.path().join("right.tif");
        fixture(&first, 2, 1, &[1u8, 2])?;
        fixture(&second, 2, 1, &[10u8, 20])?;
        let simple = |name, offset| {
            format!(
                r#"<SimpleSource><SourceFilename relativeToVRT="1">{name}</SourceFilename><SourceBand>1</SourceBand><SrcRect xOff="0" yOff="0" xSize="2" ySize="1"/><DstRect xOff="{offset}" yOff="0" xSize="2" ySize="1"/></SimpleSource>"#
            )
        };
        let path = dir.path().join("mosaic.vrt");
        std::fs::write(
            &path,
            format!(
                r#"<VRTDataset rasterXSize="4" rasterYSize="1"><VRTRasterBand dataType="Byte" band="1">{}{}</VRTRasterBand><VRTRasterBand dataType="Byte" band="2"><NoDataValue>0</NoDataValue>{}{}</VRTRasterBand></VRTDataset>"#,
                simple("left.tif", 0),
                simple("right.tif", 2),
                simple("left.tif", 0),
                simple("right.tif", 1)
            ),
        )?;
        let before = [
            std::fs::read(&path)?,
            std::fs::read(&first)?,
            std::fs::read(&second)?,
        ];
        for mode in [StatisticsMode::Exact, StatisticsMode::Approximate] {
            let report = Info::new(&path)
                .bands([2, 1])
                .statistics(mode)
                .histogram(
                    HistogramOptions::new(0.5, 20.5, 20)
                        .approximate(mode == StatisticsMode::Approximate),
                )
                .run()?;
            for (result, expected, index) in [
                (&report.value().bands[0], &[1u8, 10, 20][..], 2),
                (&report.value().bands[1], &[1u8, 2, 10, 20][..], 1),
            ] {
                assert_eq!(result.index, index);
                let stats = result.statistics.as_ref().unwrap();
                assert_eq!((stats.minimum, stats.maximum), (1., 20.));
                assert!(
                    (stats.mean
                        - expected.iter().map(|&v| f64::from(v)).sum::<f64>()
                            / expected.len() as f64)
                        .abs()
                        < 1e-9
                );
                let mut counts = vec![0; 20];
                for &value in expected {
                    counts[usize::from(value) - 1] += 1;
                }
                assert_eq!(result.histogram.as_ref().unwrap().counts, counts);
            }
        }
        for (file, bytes) in [&path, &first, &second].into_iter().zip(before) {
            assert_eq!(std::fs::read(file)?, bytes);
        }
        assert_eq!(std::fs::read_dir(dir.path())?.count(), 3);
        Ok(())
    }

    #[test]
    fn statistics_use_the_opened_overview_including_its_pixels() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("level.tif");
        super::super::test_support::fixture(&path, 4, 1, &[1u8, 2, 5, 6])?;
        let mut ds = gdal::Dataset::open_ex(
            &path,
            gdal::DatasetOptions {
                open_flags: gdal::GdalOpenFlags::GDAL_OF_UPDATE,
                ..Default::default()
            },
        )?;
        ds.build_overviews("NEAREST", &[2], &[])?;
        ds.close()?;
        let before = std::fs::read(&path)?;
        let report = Info::new(Source::new(&path).open_option("OVERVIEW_LEVEL", "0"))
            .statistics(StatisticsMode::Exact)
            .histogram(HistogramOptions::new(0.5, 6.5, 6))
            .run()?;
        assert_eq!(report.value().size, (2, 1));
        let band = &report.value().bands[0];
        assert_eq!(band.statistics.as_ref().unwrap().mean, 3.);
        assert_eq!(band.histogram.as_ref().unwrap().counts, [1, 0, 0, 0, 1, 0]);
        assert_eq!(std::fs::read(&path)?, before);
        assert_eq!(std::fs::read_dir(dir.path())?.count(), 1);
        Ok(())
    }
    #[test]
    fn statistics_keep_explicit_masks_and_dataset_nodata_tuples() -> anyhow::Result<()> {
        use gdal::raster::{Buffer, ColorInterpretation};
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("masked.tif");
        super::super::test_support::fixture(&path, 3, 1, &[4u8, 60, 80])?;
        let ds = Dataset::open_ex(
            &path,
            gdal::DatasetOptions {
                open_flags: gdal::GdalOpenFlags::GDAL_OF_UPDATE,
                ..Default::default()
            },
        )?;
        let mut band = ds.rasterband(1)?;
        band.create_mask_band(false)?;
        band.open_mask_band()?.write(
            (0, 0),
            (3, 1),
            &mut Buffer::new((3, 1), vec![255u8, 255, 0]),
        )?;
        ds.close()?;
        let inspect = |src: &std::path::Path, bands: &[usize]| -> anyhow::Result<Vec<(f64, u64)>> {
            let report = Info::new(src)
                .bands(bands.iter().copied())
                .statistics(StatisticsMode::Exact)
                .histogram(HistogramOptions::new(-0.5, 255.5, 256))
                .run()?;
            Ok(report
                .value()
                .bands
                .iter()
                .map(|b| {
                    (
                        b.statistics.as_ref().unwrap().mean,
                        b.histogram.as_ref().unwrap().counts.iter().sum(),
                    )
                })
                .collect())
        };
        assert_eq!(inspect(&path, &[1])?, [(32., 2)]);
        let ds = Dataset::open_ex(
            &path,
            gdal::DatasetOptions {
                open_flags: gdal::GdalOpenFlags::GDAL_OF_UPDATE,
                ..Default::default()
            },
        )?;
        ds.rasterband(1)?
            .set_color_interpretation(ColorInterpretation::AlphaBand)?;
        ds.close()?;
        // GDAL treats the alpha band itself as data, even when it has a mask.
        assert_eq!(inspect(&path, &[1])?, [(48., 3)]);

        let path = dir.path().join("tuple.tif");
        let mut ds = DriverManager::get_driver_by_name("GTiff")?
            .create_with_band_type::<u8, _>(&path, 3, 1, 2)?;
        ds.set_metadata_item("NODATA_VALUES", "0 0", "")?;
        for (i, pixels) in [[0, 0, 20u8], [0, 10, 30]].into_iter().enumerate() {
            ds.rasterband(i + 1)?.write(
                (0, 0),
                (3, 1),
                &mut Buffer::new((3, 1), pixels.to_vec()),
            )?;
        }
        ds.close()?;
        assert_eq!(inspect(&path, &[2, 1])?, [(20., 2), (10., 2)]);
        Ok(())
    }

    #[test]
    fn statistics_preserve_legacy_signed_bytes() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        super::super::test_support::fixture(&dir.path().join("raw.tif"), 3, 1, &[128u8, 255, 1])?;
        let path = dir.path().join("signed.vrt");
        std::fs::write(
            &path,
            r#"<VRTDataset rasterXSize="3" rasterYSize="1"><VRTRasterBand dataType="Byte" band="1"><Metadata domain="IMAGE_STRUCTURE"><MDI key="PIXELTYPE">SIGNEDBYTE</MDI></Metadata><SimpleSource><SourceFilename relativeToVRT="1">raw.tif</SourceFilename><SourceBand>1</SourceBand></SimpleSource></VRTRasterBand></VRTDataset>"#,
        )?;
        let info = Info::new(&path)
            .statistics(StatisticsMode::Exact)
            .histogram(HistogramOptions::new(-128.5, 127.5, 256))
            .run()?;
        let band = &info.value().bands[0];
        let stats = band.statistics.as_ref().unwrap();
        assert_eq!((stats.minimum, stats.maximum), (-128., 1.));
        let counts = &band.histogram.as_ref().unwrap().counts;
        assert_eq!((counts[0], counts[127], counts[129]), (1, 1, 1));
        Ok(())
    }

    #[test]
    fn statistics_preserve_native_complex_histograms() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("complex.vrt");
        super::super::test_support::fixture(&dir.path().join("real.tif"), 1, 1, &[3u8])?;
        super::super::test_support::fixture(&dir.path().join("imag.tif"), 1, 1, &[4u8])?;
        std::fs::write(
            &path,
            r#"<VRTDataset rasterXSize="1" rasterYSize="1"><VRTRasterBand dataType="CFloat32" band="1" subClass="VRTDerivedRasterBand"><PixelFunctionType>complex</PixelFunctionType><SimpleSource><SourceFilename relativeToVRT="1">real.tif</SourceFilename><SourceBand>1</SourceBand></SimpleSource><SimpleSource><SourceFilename relativeToVRT="1">imag.tif</SourceFilename><SourceBand>1</SourceBand></SimpleSource></VRTRasterBand></VRTDataset>"#,
        )?;
        let truth = dir.path().join("truth.vrt");
        std::fs::copy(&path, &truth)?;
        let native = Dataset::open(&truth)?;
        let band = native.rasterband(1)?;
        let expected = band.histogram(-0.5, 5.5, 6, false, false)?;
        let result = Info::new(&path)
            .histogram(HistogramOptions::new(-0.5, 5.5, 6))
            .run()?;
        assert_eq!(
            result.value().bands[0].histogram.as_ref().unwrap().counts,
            expected.counts()
        );
        Ok(())
    }

    #[test]
    fn wide_integer_statistics_cannot_change_nodata_membership() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("wide.tif");
        let ds = DriverManager::get_driver_by_name("GTiff")?
            .create_with_band_type::<u64, _>(&path, 2, 1, 1)?;
        let nodata = 9_007_199_254_740_993u64;
        ds.rasterband(1)?.write(
            (0, 0),
            (2, 1),
            &mut gdal::raster::Buffer::new((2, 1), vec![nodata - 1, nodata]),
        )?;
        ds.rasterband(1)?.set_no_data_value_u64(Some(nodata))?;
        ds.close()?;
        let before = std::fs::read(&path)?;
        assert_eq!(
            Info::new(&path).run()?.value().bands[0].nodata,
            Some(super::super::PixelValue::UInt(nodata))
        );
        let error = Info::new(&path)
            .statistics(StatisticsMode::Exact)
            .run()
            .unwrap_err();
        assert_eq!(error.kind(), crate::operation::ErrorKind::Unsupported);
        assert_eq!(std::fs::read(&path)?, before);
        assert_eq!(std::fs::read_dir(dir.path())?.count(), 1);
        Ok(())
    }
    #[test]
    fn existing_derived_functions_and_read_only_callback_failures_are_preserved()
    -> anyhow::Result<()> {
        use crate::operation::{ErrorKind, OutputState};
        let dir = tempfile::tempdir()?;
        let source = dir.path().join("pixels.tif");
        super::super::test_support::fixture(&source, 2, 1, &[1u8, 2])?;
        let path = dir.path().join("derived.vrt");
        // Older GDAL releases require two sources for the sum pixel function.
        let input = format!(
            r#"<SimpleSource><SourceFilename relativeToVRT="0">{}</SourceFilename><SourceBand>1</SourceBand></SimpleSource>"#,
            source.display()
        );
        std::fs::write(
            &path,
            format!(
                r#"<VRTDataset rasterXSize="2" rasterYSize="1"><VRTRasterBand dataType="Float64" band="1" subClass="VRTDerivedRasterBand"><PixelFunctionType>sum</PixelFunctionType><PixelFunctionArguments k="10"/>{input}{input}</VRTRasterBand></VRTDataset>"#
            ),
        )?;
        let before = std::fs::read(&source)?;
        let result = Info::new(&path).statistics(StatisticsMode::Exact).run()?;
        let stats = result.value().bands[0].statistics.as_ref().unwrap();
        assert_eq!((stats.minimum, stats.maximum), (12., 14.));
        for panic in [false, true] {
            let mut callback = |_: &crate::operation::Progress| {
                assert!(!panic, "test callback panic");
                std::ops::ControlFlow::Break(())
            };
            let error = Info::new(&path)
                .statistics(StatisticsMode::Exact)
                .run_with(&mut Control::default().on_progress(&mut callback))
                .unwrap_err();
            assert_eq!(
                error.kind(),
                if panic {
                    ErrorKind::CallbackPanicked
                } else {
                    ErrorKind::Cancelled
                }
            );
            assert_eq!(error.output_state(), OutputState::NotModified);
        }
        assert_eq!(std::fs::read(&source)?, before);
        assert_eq!(std::fs::read_dir(dir.path())?.count(), 2);
        Ok(())
    }
    #[test]
    fn pam_georeferencing_masks_and_vrt_inputs_remain_read_only() -> anyhow::Result<()> {
        use gdal::{Dataset, raster::Buffer};
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("masked.tif");
        super::super::test_support::fixture(&path, 2, 2, &[0u8, 2, 10, 20])?;
        let ds = Dataset::open_ex(
            &path,
            gdal::DatasetOptions {
                open_flags: gdal::GdalOpenFlags::GDAL_OF_UPDATE,
                ..Default::default()
            },
        )?;
        let mut band = ds.rasterband(1)?;
        band.set_no_data_value(Some(0.))?;
        band.set_scale(2.)?;
        band.set_offset(100.)?;
        band.create_mask_band(false)?;
        band.open_mask_band()?.write(
            (0, 0),
            (2, 2),
            &mut Buffer::new((2, 2), vec![0u8, 255, 255, 0]),
        )?;
        ds.close()?;
        let pam = dir.path().join("masked.tif.aux.xml");
        std::fs::write(
            &pam,
            "<PAMDataset><SRS>EPSG:3857</SRS><GeoTransform>100,2,0,200,0,-2</GeoTransform></PAMDataset>",
        )?;
        let before = std::fs::read_dir(dir.path())?
            .map(|p| {
                let p = p?.path();
                Ok((p.clone(), std::fs::read(p)?))
            })
            .collect::<std::io::Result<Vec<_>>>()?;
        let result = Info::new(Source::new(&path).open_option("GEOREF_SOURCES", "PAM"))
            .statistics(StatisticsMode::Exact)
            .histogram(HistogramOptions::new(-0.5, 20.5, 21))
            .run()?;
        assert_eq!(
            result.value().geo_transform,
            Some([100., 2., 0., 200., 0., -2.])
        );
        assert_eq!(result.value().bands[0].scale, Some(2.));
        let statistics = result.value().bands[0].statistics.as_ref().unwrap();
        let native_dir = tempfile::tempdir()?;
        let native_path = native_dir.path().join("native.tif");
        std::fs::copy(&path, &native_path)?;
        let mask = dir.path().join("masked.tif.msk");
        if mask.exists() {
            std::fs::copy(mask, native_dir.path().join("native.tif.msk"))?;
        }
        let native = Dataset::open(&native_path)?;
        let expected = native.rasterband(1)?.get_statistics(true, false)?.unwrap();
        native.close()?;
        assert_eq!(statistics.minimum, expected.min);
        assert_eq!(statistics.maximum, expected.max);
        let sample = super::super::PixelInfo::new(
            &path,
            super::super::SampleLocation::Pixel { column: 1, row: 1 },
        )
        .run()?;
        assert!(!sample.value().bands[0].valid);
        for (path, bytes) in &before {
            assert_eq!(std::fs::read(path)?, *bytes);
        }
        assert_eq!(std::fs::read_dir(dir.path())?.count(), before.len());
        let vrt = dir.path().join("source.vrt");
        let _ = super::super::Translate::new(&path, &vrt)
            .driver("VRT")
            .run()?;
        let bytes = std::fs::read(&vrt)?;
        let result = Info::new(&vrt)
            .statistics(StatisticsMode::Approximate)
            .run()?;
        assert_eq!(
            result.value().bands[0].statistics.as_ref().unwrap().minimum,
            2.
        );
        assert_eq!(std::fs::read(&vrt)?, bytes);
        Ok(())
    }
    #[test]
    fn statistics_and_histogram_leave_source_and_sidecars_unchanged() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("source.tif");
        super::super::test_support::fixture(&path, 2, 2, &[1u8, 2, 3, 4])?;
        let before = std::fs::read(&path)?;
        let outcome = Info::new(&path)
            .statistics(StatisticsMode::Exact)
            .histogram(HistogramOptions::new(0.5, 4.5, 4))
            .run()?;
        let band = &outcome.value().bands[0];
        assert_eq!(band.statistics.as_ref().unwrap().mean, 2.5);
        assert_eq!(band.histogram.as_ref().unwrap().counts, [1, 1, 1, 1]);
        assert_eq!(std::fs::read(&path)?, before);
        assert_eq!(
            std::fs::read_dir(dir.path())?.count(),
            1,
            "inspection wrote a sidecar"
        );
        Ok(())
    }
}

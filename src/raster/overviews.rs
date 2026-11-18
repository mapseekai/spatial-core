use super::{Resampling, options};
use crate::{
    config::gdal::GdalOptions,
    dataset::{Kind, Options, Source},
    gdal_ext::{
        algorithm::{Algorithm, Argument},
        destination::DatasetDestination,
        diagnostics::Diagnostics,
        execution::{close, supported_version},
        ffi::{checked_gdal_call, last_null_pointer_err},
        progress::ProgressCallbackState,
        query::{self, inspect_error},
        raster::translate::{TranslateOptions, translate},
    },
    operation::{Control, Error, ErrorKind, OutputState, Report, Result, Stage},
};
use gdal::{Dataset, Metadata, raster::RasterBand};
use std::{
    collections::HashSet,
    ffi::CString,
    path::{Path, PathBuf},
};

/// Explicit location to maintain; no fallback to another storage location.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OverviewStorage {
    /// Read the target and maintain its .ovr sidecar.
    #[default]
    External,
    /// Modify a regular non-COG GeoTIFF in place.
    Internal,
}
#[derive(Debug, Clone)]
struct Maintenance {
    target: Source,
    storage: OverviewStorage,
    levels: Vec<u32>,
    bands: Vec<usize>,
    resampling: Resampling,
    creation: Options,
    config: GdalOptions,
}
impl Maintenance {
    fn new(target: impl Into<Source>) -> Self {
        Self {
            target: target.into(),
            storage: OverviewStorage::External,
            levels: Vec::new(),
            bands: Vec::new(),
            resampling: Resampling::Nearest,
            creation: Options::default(),
            config: GdalOptions::default(),
        }
    }
    fn run(self, action: Action, control: &mut Control<'_>) -> Result<Report> {
        supported_version()?;
        self.target.validate()?;
        if self.target.open_option_value("OVERVIEW_LEVEL").is_some() {
            return Err(Error::invalid(
                "overview maintenance must open the base dataset, without OVERVIEW_LEVEL",
            ));
        }
        self.creation.validate()?;
        control.check()?;
        self.resampling.validate_translate()?;
        if let Action::Refresh(Some(bounds)) = action {
            options::bounds(bounds)?;
        }
        let mut unique = HashSet::new();
        if self
            .levels
            .iter()
            .any(|&v| v <= 1 || v > i32::MAX as u32 || !unique.insert(v))
        {
            return Err(Error::invalid(
                "overview levels must be unique integers in 2..=i32::MAX",
            ));
        }
        if matches!(action, Action::Build) && self.levels.is_empty() {
            return Err(Error::invalid("overview levels are required"));
        }
        let path = self.target.local_path().ok_or_else(|| {
            Error::unsupported("overview maintenance requires a local target path")
        })?;
        check_files(path)?;
        let diagnostics = Diagnostics::enter();
        let scope = self.config.enter();
        // Enforce the declared storage contract even when process configuration requests RRD.
        let storage_scope = GdalOptions::default()
            .set("USE_RRD", "NO")
            .expect("static option")
            .set("HFA_USE_RRD", "NO")
            .expect("static option")
            .enter();
        let mut state = OutputState::NotModified;
        let mut target = None;
        let mut sidecar_output = None;
        let mut mask_output = None;
        let mut external_mask = false;
        let result = (|| {
            target = Some(self.target.open_kind(Kind::Raster)?);
            let dataset = target.as_ref().expect("target open");
            let driver = dataset.driver().short_name();
            if driver != "GTiff" && !(driver == "VRT" && self.storage == OverviewStorage::External)
            {
                return Err(Error::unsupported(
                    "overview maintenance supports GTiff and external VRT overviews",
                ));
            }
            if self.storage == OverviewStorage::Internal
                && dataset
                    .metadata_item("LAYOUT", "IMAGE_STRUCTURE")
                    .as_deref()
                    == Some("COG")
            {
                return Err(Error::unsupported(
                    "internal modification of a COG is refused; create a new COG",
                ));
            }
            if dataset.raster_count() == 0 {
                return Err(Error::invalid("target has no raster bands"));
            }
            if !self.bands.is_empty() {
                let mut selected = self.bands.clone();
                selected.sort_unstable();
                selected.dedup();
                if selected != (1..=dataset.raster_count()).collect::<Vec<_>>() {
                    return Err(Error::unsupported(
                        "these overview drivers require all bands",
                    ));
                }
            }
            let band = dataset.rasterband(1).map_err(inspect_error)?;
            let sizes = (0..band.overview_count().map_err(inspect_error)?)
                .map(|i| {
                    band.overview(i as usize)
                        .map(|b| b.size())
                        .map_err(inspect_error)
                })
                .collect::<Result<Vec<_>>>()?;
            for index in 2..=dataset.raster_count() {
                let band = dataset.rasterband(index).map_err(inspect_error)?;
                let other = (0..band.overview_count().map_err(inspect_error)?)
                    .map(|i| {
                        band.overview(i as usize)
                            .map(|b| b.size())
                            .map_err(inspect_error)
                    })
                    .collect::<Result<Vec<_>>>()?;
                if other != sizes {
                    return Err(Error::unsupported(
                        "overview levels must agree across all bands",
                    ));
                }
            }
            let external = sidecar(path, ".ovr").exists();
            if self.storage == OverviewStorage::Internal && external {
                return Err(Error::unsupported(
                    "external overviews exist; storage location is ambiguous",
                ));
            }
            if self.storage == OverviewStorage::External && !sizes.is_empty() && !external {
                return Err(Error::unsupported(
                    "internal or implicit overviews already exist; storage location is ambiguous",
                ));
            }
            if external && !sizes.is_empty() {
                let overview = band.overview(0).map_err(inspect_error)?;
                let owner = owner_description(&overview)?;
                if !owner.is_empty()
                    && native_overview_path(owner)?.canonicalize().ok() == path.canonicalize().ok()
                {
                    return Err(Error::unsupported(
                        "mixed internal and external overviews are not supported",
                    ));
                }
            }
            let refresh = if let Action::Refresh(bounds) = action {
                if sizes.is_empty() {
                    return Err(Error::invalid("there are no overviews to refresh"));
                }
                let (w, h) = dataset.raster_size();
                let selected: HashSet<_> = self
                    .levels
                    .iter()
                    .map(|&n| (w.div_ceil(n as usize), h.div_ceil(n as usize)))
                    .collect();
                if selected.iter().any(|size| !sizes.contains(size)) {
                    return Err(Error::invalid(
                        "refresh cannot create a missing overview level",
                    ));
                }
                let slots: Vec<_> = sizes
                    .iter()
                    .enumerate()
                    .filter(|(_, size)| selected.is_empty() || selected.contains(size))
                    .map(|(i, _)| i)
                    .collect();
                if self.storage == OverviewStorage::External {
                    verify_external_slots(dataset, &sidecar(path, ".ovr"), &slots, false)?;
                    external_mask =
                        verify_external_slots(dataset, &sidecar(path, ".msk.ovr"), &slots, true)?;
                } else {
                    for n in 1..=dataset.raster_count() {
                        let b = dataset.rasterband(n).map_err(inspect_error)?;
                        verify_internal_slots(&b, path, &slots)?;
                        if has_stored_mask(&b)? {
                            let msk = sidecar(path, ".msk");
                            verify_internal_slots(
                                &b.open_mask_band().map_err(inspect_error)?,
                                if msk.exists() { &msk } else { path },
                                &slots,
                            )?;
                        }
                    }
                }
                let region = bounds
                    .map(|bbox| region_window(dataset, bbox))
                    .transpose()?;
                Some((slots, region))
            } else {
                None
            };
            if let Some((_, Some([x0, y0, x1, y1]))) = &refresh
                && (x0 >= x1 || y0 >= y1)
            {
                return Ok(());
            }
            if self.storage == OverviewStorage::Internal {
                let mut failure = None;
                close(target.take(), Stage::CloseSource, &mut failure);
                if let Some(error) = failure {
                    return Err(error);
                }
                control.check()?;
                state = OutputState::MayHaveChanged;
                target = Some(self.target.open_update(Kind::Raster)?);
            }
            query::invoke(control, |progress| {
                let dataset = target.as_ref().expect("target open");
                query::pulse(progress, 0., c"maintaining overviews")?;
                if let Some((slots, region)) = &refresh {
                    if self.storage == OverviewStorage::External {
                        state = OutputState::MayHaveChanged;
                        sidecar_output = Some(
                            Source::new(sidecar(path, ".ovr"))
                                .allowed_drivers(["GTiff"])
                                .open_update(Kind::Raster)?,
                        );
                        if external_mask {
                            mask_output = Some(
                                Source::new(sidecar(path, ".msk.ovr"))
                                    .allowed_drivers(["GTiff"])
                                    .open_update(Kind::Raster)?,
                            );
                        }
                    }
                    return refresh_slots(
                        dataset,
                        sidecar_output.as_ref(),
                        mask_output.as_ref(),
                        slots,
                        *region,
                        self.resampling,
                        progress,
                    );
                }
                let levels: Vec<i32> = self.levels.iter().map(|&v| v as i32).collect();
                let resampling = CString::new(resampling(self.resampling)).expect("static name");
                let mut list = gdal::cpl::CslStringList::new();
                for value in self.creation.strings() {
                    list.add_string(&value).map_err(inspect_error)?;
                }
                state = OutputState::MayHaveChanged;
                // SAFETY: target, validated level slice, options and callback remain live.
                let result = checked_gdal_call(|| unsafe {
                    gdal_sys::GDALBuildOverviewsEx(
                        dataset.c_dataset(),
                        resampling.as_ptr(),
                        levels.len() as i32,
                        levels.as_ptr(),
                        0,
                        std::ptr::null(),
                        progress.native_callback(),
                        progress.user_data(),
                        list.as_ptr(),
                    )
                });
                query::check_progress(progress)?;
                let status = result.map_err(|e| Error::native(Stage::Execute, e))?;
                if status != gdal_sys::CPLErr::CE_None {
                    return Err(Error::native(
                        Stage::Execute,
                        last_null_pointer_err("GDALBuildOverviewsEx"),
                    ));
                }
                Ok(())
            })
        })();
        let mut failure = result.err();
        close(mask_output, Stage::CloseOutput, &mut failure);
        close(sidecar_output, Stage::CloseOutput, &mut failure);
        close(
            target,
            if self.storage == OverviewStorage::Internal {
                Stage::CloseOutput
            } else {
                Stage::CloseSource
            },
            &mut failure,
        );
        if failure.is_none() && diagnostics.failures() != 0 {
            failure = Some(Error::new(
                ErrorKind::Processing,
                Stage::Execute,
                "GDAL reported an overview failure",
            ));
        }
        drop(storage_scope);
        drop(scope);
        let report = diagnostics.take_report();
        if let Some(mut error) = failure {
            error.output_state = state;
            error.report = report;
            Err(error)
        } else {
            Ok(report)
        }
    }
}
fn has_stored_mask(band: &RasterBand<'_>) -> Result<bool> {
    let flags = band.mask_flags().map_err(inspect_error)?;
    Ok(!flags.is_all_valid()
        && !flags.is_nodata()
        && !flags.is_alpha()
        && band
            .open_mask_band()
            .map_err(inspect_error)?
            .overview_count()
            .map_err(inspect_error)?
            > 0)
}

fn owner_description<'a>(band: &'a RasterBand<'_>) -> Result<&'a std::ffi::CStr> {
    // SAFETY: the description is borrowed from the live parent of this band.
    unsafe {
        let ds = gdal_sys::GDALGetBandDataset(band.c_rasterband());
        if !ds.is_null() {
            let desc = gdal_sys::GDALGetDescription(ds);
            if !desc.is_null() {
                return Ok(std::ffi::CStr::from_ptr(desc));
            }
        }
    }
    Err(Error::unsupported("unknown overview owner"))
}

fn native_overview_path(value: &std::ffi::CStr) -> Result<&Path> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(Path::new(std::ffi::OsStr::from_bytes(value.to_bytes())))
    }
    #[cfg(not(unix))]
    {
        value
            .to_str()
            .map(Path::new)
            .map_err(|e| Error::unsupported("invalid native overview filename").with_source(e))
    }
}
fn verify_internal_slots(base: &RasterBand<'_>, path: &Path, slots: &[usize]) -> Result<()> {
    let expected = path
        .canonicalize()
        .map_err(|e| Error::invalid("cannot resolve overview target").with_source(e))?;
    for slot in std::iter::once(0).chain(slots.iter().copied().filter(|&i| i != 0)) {
        let band = base.overview(slot).map_err(inspect_error)?;
        let owner = owner_description(&band)?;
        // GTiff exposes its internal IFDs as unnamed datasets; a named owner must be the target file.
        if !owner.is_empty()
            && native_overview_path(owner)?.canonicalize().ok().as_deref()
                != Some(expected.as_path())
        {
            return Err(Error::unsupported(
                "requested internal overview belongs to an external dataset",
            ));
        }
    }
    Ok(())
}

fn verify_external_slots(
    source: &Dataset,
    path: &Path,
    slots: &[usize],
    mask: bool,
) -> Result<bool> {
    let mut found = false;
    for n in 1..=source.raster_count() {
        let band = source.rasterband(n).map_err(inspect_error)?;
        if mask && !has_stored_mask(&band)? {
            continue;
        }
        let mask_band;
        let base = if mask {
            mask_band = band.open_mask_band().map_err(inspect_error)?;
            &mask_band
        } else {
            &band
        };
        let first = base.overview(0).map_err(inspect_error)?;
        let owner = owner_description(&first)?;
        let canonical = path
            .canonicalize()
            .map_err(|_| Error::unsupported("selected external overview sidecar is unavailable"))?;
        if owner.is_empty()
            || native_overview_path(owner)?.canonicalize().ok().as_deref()
                != Some(canonical.as_path())
        {
            return Err(Error::unsupported(
                "overview belongs to a different physical file",
            ));
        }
        // Slot zero is the physical sidecar's base band; later slots must be its own internal IFDs.
        for &slot in slots {
            let logical = base.overview(slot).map_err(inspect_error)?;
            if slot != 0 {
                let physical = first.overview(slot - 1).map_err(inspect_error)?;
                let parent = owner_description(&physical)?;
                // SAFETY: both borrowed bands are held by the live source.
                if unsafe { logical.c_rasterband() != physical.c_rasterband() }
                    || (!parent.is_empty() && parent != owner)
                {
                    return Err(Error::unsupported(
                        "overview slot is not internal to the selected sidecar",
                    ));
                }
            }
            if mask && logical.size() != band.overview(slot).map_err(inspect_error)?.size() {
                return Err(Error::unsupported(
                    "overview mask geometry differs from its data slot",
                ));
            }
        }
        found = true;
    }
    Ok(found)
}

fn region_window(dataset: &Dataset, bounds: [f64; 4]) -> Result<[usize; 4]> {
    if supported_version()? < 3_120_000 {
        return Err(Error::unsupported(
            "regional overview refresh requires GDAL 3.12 or later",
        ));
    }
    let gt = dataset.geo_transform().map_err(inspect_error)?;
    if gt[2] != 0. || gt[4] != 0. || gt[1] == 0. || gt[5] == 0. || !gt.iter().all(|v| v.is_finite())
    {
        return Err(Error::unsupported(
            "regional overview refresh requires a non-rotated invertible affine grid",
        ));
    }
    let (width, height) = dataset.raster_size();
    let interval = |min, max, origin, scale, size: usize| {
        let a: f64 = (min - origin) / scale;
        let b: f64 = (max - origin) / scale;
        (
            (a.min(b) + 1e-8).floor().clamp(0., size as f64) as usize,
            (a.max(b) - 1e-8).ceil().clamp(0., size as f64) as usize,
        )
    };
    let (x0, x1) = interval(bounds[0], bounds[2], gt[0], gt[1], width);
    let (y0, y1) = interval(bounds[1], bounds[3], gt[3], gt[5], height);
    Ok([x0, y0, x1, y1])
}

fn refresh_slots(
    input: &Dataset,
    external: Option<&Dataset>,
    masks: Option<&Dataset>,
    slots: &[usize],
    region: Option<[usize; 4]>,
    method: Resampling,
    progress: &mut ProgressCallbackState<'_>,
) -> Result<()> {
    let method = resampling(method);
    fn slot_band(ds: &Dataset, n: usize, slot: usize) -> Result<RasterBand<'_>> {
        let b = ds.rasterband(n).map_err(inspect_error)?;
        if slot == 0 {
            Ok(b)
        } else {
            b.overview(slot - 1).map_err(inspect_error)
        }
    }
    let mut seen_masks = HashSet::new();
    for n in 1..=input.raster_count() {
        let src = input.rasterband(n).map_err(inspect_error)?;
        let mask = if has_stored_mask(&src)? {
            let mask = src.open_mask_band().map_err(inspect_error)?;
            // SAFETY: the input owns the mask throughout this operation; shared masks need only one refresh.
            seen_masks
                .insert(unsafe { mask.c_rasterband() })
                .then_some(mask)
        } else {
            None
        };
        for &slot in slots {
            let existing = src.overview(slot).map_err(inspect_error)?;
            let dest = if let Some(e) = external {
                slot_band(e, n, slot)?
            } else {
                existing
            };
            refresh_band(input, &n.to_string(), &src, &dest, region, method, progress)?;
            if let Some(mask) = &mask {
                let output = if let Some(masks) = masks {
                    let root = mask.overview(0).map_err(inspect_error)?;
                    // SAFETY: the root is a verified physical band of the external mask sidecar.
                    let number =
                        unsafe { gdal_sys::GDALGetBandNumber(root.c_rasterband()) } as usize;
                    slot_band(masks, number, slot)?
                } else {
                    mask.overview(slot).map_err(inspect_error)?
                };
                refresh_band(
                    input,
                    &format!("mask,{n}"),
                    mask,
                    &output,
                    region,
                    method,
                    progress,
                )?;
            }
        }
    }
    Ok(())
}

fn with_dataset(
    dataset: Dataset,
    stage: Stage,
    work: impl FnOnce(&Dataset) -> Result<()>,
) -> Result<()> {
    let mut failure = work(&dataset).err();
    close(Some(dataset), stage, &mut failure);
    failure.map_or(Ok(()), Err)
}

fn refresh_band(
    input: &Dataset,
    selection: &str,
    src: &RasterBand<'_>,
    dest: &RasterBand<'_>,
    region: Option<[usize; 4]>,
    method: &str,
    progress: &mut ProgressCallbackState<'_>,
) -> Result<()> {
    let (sw, sh) = src.size();
    let (dw, dh) = dest.size();
    let Some([x0, y0, x1, y1]) = region else {
        return regenerate(src, dest, method, progress);
    };
    // Use the same pixel window rounding as GDAL's regional overview algorithm.
    let start =
        |i: usize, d: usize, s: usize| (i as f64 / s as f64 * d as f64 + 1e-8).floor() as usize;
    let end = |i: usize, d: usize, s: usize| {
        ((i as f64 / s as f64 * d as f64 - 1e-8).ceil() as usize).min(d)
    };
    let window = [
        start(x0, dw, sw),
        start(y0, dh, sh),
        end(x1, dw, sw),
        end(y1, dh, sh),
    ];
    if window[0] >= window[2] || window[1] >= window[3] {
        return Ok(());
    }
    if window == [0, 0, dw, dh] {
        return regenerate(src, dest, method, progress);
    }
    let mut args = vec!["-of", "VRT", "-b", selection, "-ovr", "NONE"];
    let mask_arg = format!("mask,{selection}");
    if selection.starts_with("mask,") {
        args.extend(["-a_nodata", "0"]);
    } else if !src.mask_flags().map_err(inspect_error)?.is_all_valid() {
        // Selecting an RGB band alone would otherwise lose a sibling alpha band's validity.
        args.extend(["-mask", &mask_arg]);
    }
    if src.color_interpretation() == gdal::raster::ColorInterpretation::PaletteIndex
        && matches!(method, "average" | "rms" | "mode")
        && src.color_table().is_some()
    {
        // A composed VRT must apply its own palette, not forward resampling to an unpaletted child.
        let _band_io = GdalOptions::default()
            .set("GDAL_FORCE_CACHING", "YES")
            .expect("static option")
            .enter();
        // Single-band RasterIO uses colors; the regional multiband kernel averages raw indexes.
        let (width, height) = (dw.to_string(), dh.to_string());
        args.extend(["-outsize", &width, &height, "-r", method]);
        let proxy = translate(
            input,
            DatasetDestination::path("").map_err(inspect_error)?,
            Some(TranslateOptions::new(args).map_err(inspect_error)?),
        )
        .map_err(|e| Error::native(Stage::Execute, e))?;
        return with_dataset(proxy, Stage::CloseSource, |ds| {
            copy_window(
                &ds.rasterband(1).map_err(inspect_error)?,
                dest,
                window,
                progress,
            )
        });
    }
    let configured = gdal::config::get_config_option("CPL_TMPDIR", "").map_err(inspect_error)?;
    let scratch = if configured.is_empty() {
        tempfile::tempdir()
    } else {
        tempfile::tempdir_in(configured)
    }
    .map_err(|e| Error::invalid("cannot create temporary overview storage").with_source(e))?;
    let vrt = scratch.path().join("source.vrt");
    let staged = sidecar(&vrt, ".ovr");
    // Native regional refresh addresses levels by a rounded factor; a private single-slot view makes it unambiguous.
    let path = crate::dataset::local_name(&staged)?;
    let options = gdal::raster::RasterCreationOptions::from_iter([
        "TILED=YES",
        "SPARSE_OK=YES",
        "BIGTIFF=IF_SAFER",
    ]);
    let driver = gdal::DriverManager::get_driver_by_name("GTiff").map_err(inspect_error)?;
    // SAFETY: the driver, name and options live for this call; the dimensions and type come from a live GDAL band.
    let raw = unsafe {
        gdal_sys::GDALCreate(
            driver.c_driver(),
            path.as_ptr(),
            dw as i32,
            dh as i32,
            1,
            gdal_sys::GDALGetRasterDataType(dest.c_rasterband()),
            options.as_ptr(),
        )
    };
    if raw.is_null() {
        return Err(Error::native(
            Stage::Execute,
            last_null_pointer_err("GDALCreate(temporary overview)"),
        ));
    }
    // SAFETY: GDALCreate transferred this fresh dataset to us.
    let stage = unsafe { Dataset::from_c_dataset(raw) };
    with_dataset(stage, Stage::CloseOutput, |_| Ok(()))?;
    args.extend(["-a_gt", "0", "1", "0", "0", "0", "1"]);
    let opts = TranslateOptions::new(args).map_err(inspect_error)?;
    let proxy = translate(
        input,
        DatasetDestination::path(&vrt).map_err(inspect_error)?,
        Some(opts),
    )
    .map_err(|e| Error::native(Stage::Execute, e))?;
    with_dataset(proxy, Stage::CloseOutput, |_| Ok(()))?;
    with_dataset(
        Source::new(&vrt).open_update(Kind::Raster)?,
        Stage::CloseOutput,
        |proxy| {
            let band = proxy.rasterband(1).map_err(inspect_error)?;
            if band.overview_count().map_err(inspect_error)? != 1
                || band.overview(0).map_err(inspect_error)?.size() != (dw, dh)
            {
                return Err(Error::unsupported(
                    "regional overview view has ambiguous levels",
                ));
            }
            verify_external_slots(proxy, &staged, &[0], false)?;
            let mut algorithm = Algorithm::new(&["raster", "overview", "refresh"])?;
            algorithm.bind("dataset", proxy)?;
            algorithm.set(
                "bbox",
                Argument::Doubles(&[x0 as f64, y0 as f64, x1 as f64, y1 as f64]),
            )?;
            algorithm.set("resampling", Argument::String(method))?;
            algorithm.execute(progress)
        },
    )?;
    with_dataset(
        Source::new(staged).open_kind(Kind::Raster)?,
        Stage::CloseSource,
        |stage| {
            copy_window(
                &stage.rasterband(1).map_err(inspect_error)?,
                dest,
                window,
                progress,
            )
        },
    )
}

fn copy_window(
    src: &RasterBand<'_>,
    dest: &RasterBand<'_>,
    [x0, y0, x1, y1]: [usize; 4],
    progress: &mut ProgressCallbackState<'_>,
) -> Result<()> {
    // SAFETY: both are live bands of the same native type.
    let (dt, bytes) = unsafe {
        let dt = gdal_sys::GDALGetRasterDataType(src.c_rasterband());
        (dt, gdal_sys::GDALGetDataTypeSizeBytes(dt) as usize)
    };
    let (bx, by) = src.block_size();
    let mut pixels = vec![0u8; bx * by * bytes];
    for y in (y0..y1).step_by(by) {
        for x in (x0..x1).step_by(bx) {
            let (w, h) = ((x1 - x).min(bx) as i32, (y1 - y).min(by) as i32);
            for (band, rw) in [
                (src, gdal_sys::GDALRWFlag::GF_Read),
                (dest, gdal_sys::GDALRWFlag::GF_Write),
            ] {
                // SAFETY: pixels has at least w*h elements of the exact native type; offsets are band bounded.
                let status = checked_gdal_call(|| unsafe {
                    gdal_sys::GDALRasterIO(
                        band.c_rasterband(),
                        rw,
                        x as i32,
                        y as i32,
                        w,
                        h,
                        pixels.as_mut_ptr().cast(),
                        w,
                        h,
                        dt,
                        0,
                        0,
                    )
                })
                .map_err(|e| Error::native(Stage::Execute, e))?;
                if status != gdal_sys::CPLErr::CE_None {
                    return Err(Error::native(
                        Stage::Execute,
                        last_null_pointer_err("copying overview region"),
                    ));
                }
            }
        }
        query::pulse(
            progress,
            (y + by - y0).min(y1 - y0) as f64 / (y1 - y0) as f64,
            c"refreshing overview region",
        )?;
    }
    Ok(())
}
fn regenerate(
    src: &RasterBand<'_>,
    dest: &RasterBand<'_>,
    method: &str,
    progress: &mut ProgressCallbackState<'_>,
) -> Result<()> {
    let method = CString::new(method).expect("static resampling");
    // SAFETY: these two band handles and the callback are live throughout this synchronous C call.
    let response = checked_gdal_call(|| unsafe {
        gdal_sys::GDALRegenerateOverviews(
            src.c_rasterband(),
            1,
            &mut dest.c_rasterband(),
            method.as_ptr(),
            progress.native_callback(),
            progress.user_data(),
        )
    });
    query::check_progress(progress)?;
    let status = response.map_err(|e| Error::native(Stage::Execute, e))?;
    if status != gdal_sys::CPLErr::CE_None {
        return Err(Error::native(
            Stage::Execute,
            last_null_pointer_err("GDALRegenerateOverviews"),
        ));
    }
    Ok(())
}
#[derive(Clone, Copy)]
enum Action {
    Build,
    Clear,
    Refresh(Option<[f64; 4]>),
}
fn resampling(value: Resampling) -> &'static str {
    if value == Resampling::Nearest {
        "nearest"
    } else {
        value.name()
    }
}
fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}
fn check_files(path: &Path) -> Result<()> {
    for path in [
        path.to_path_buf(),
        sidecar(path, ".ovr"),
        sidecar(path, ".aux.xml"),
        sidecar(path, ".ovr.aux.xml"),
        sidecar(path, ".msk"),
        sidecar(path, ".msk.ovr"),
        sidecar(path, ".msk.aux.xml"),
        sidecar(path, ".msk.ovr.aux.xml"),
    ] {
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if !metadata.is_file() || metadata.file_type().is_symlink() {
                    return Err(Error::unsupported(
                        "overview target and sidecars must be regular, non-symlink files",
                    ));
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    if metadata.nlink() > 1 {
                        return Err(Error::unsupported(
                            "hard-linked overview targets or sidecars are ambiguous",
                        ));
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(
                    Error::invalid("cannot inspect overview file ownership").with_source(error)
                );
            }
        }
    }
    Ok(())
}
macro_rules! common {
    () => {
        /// Select Internal or External storage; default External.
        pub fn storage(mut self, storage: OverviewStorage) -> Self {
            self.options.storage = storage;
            self
        }
        /// Scope GDAL configuration through target close.
        pub fn gdal_options(mut self, options: GdalOptions) -> Self {
            self.options.config = options;
            self
        }
    };
}
macro_rules! sampling {
    () => {
        /// Select decimation factors greater than one.
        pub fn levels(mut self, levels: impl IntoIterator<Item = u32>) -> Self {
            self.options.levels = levels.into_iter().collect();
            self
        }
        /// Select all one-based bands explicitly; partial sets are refused.
        pub fn bands(mut self, bands: impl IntoIterator<Item = usize>) -> Self {
            self.options.bands = bands.into_iter().collect();
            self
        }
        /// Select native overview resampling.
        pub fn resampling(mut self, value: Resampling) -> Self {
            self.options.resampling = value;
            self
        }
    };
}
/// Create/rebuild explicitly selected levels. Existing overview files may change.
#[derive(Debug, Clone)]
#[must_use]
pub struct BuildOverviews {
    options: Maintenance,
}
impl BuildOverviews {
    /// Describe the existing target to maintain.
    pub fn new(target: impl Into<Source>) -> Self {
        Self {
            options: Maintenance::new(target),
        }
    }
    common!();
    sampling!();
    /// Set an overview creation option, such as COMPRESS.
    pub fn overview_option(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.options.creation.set(key.into(), value.into());
        self
    }
    /// Maintain synchronously.
    pub fn run(self) -> Result<Report> {
        self.run_with(&mut Control::default())
    }
    /// Maintain with borrowed progress/cancellation; failure does not roll back.
    pub fn run_with(self, control: &mut Control<'_>) -> Result<Report> {
        self.options.run(Action::Build, control)
    }
}
/// Remove all levels in the explicitly selected storage location.
#[derive(Debug, Clone)]
#[must_use]
pub struct ClearOverviews {
    options: Maintenance,
}
impl ClearOverviews {
    /// Describe the existing target to maintain.
    pub fn new(target: impl Into<Source>) -> Self {
        Self {
            options: Maintenance::new(target),
        }
    }
    common!();
    /// Maintain synchronously.
    pub fn run(self) -> Result<Report> {
        self.run_with(&mut Control::default())
    }
    /// Maintain with borrowed progress/cancellation; failure does not roll back.
    pub fn run_with(self, control: &mut Control<'_>) -> Result<Report> {
        self.options.run(Action::Clear, control)
    }
}
/// Recompute existing physical overview slots without creating levels.
/// Omitting levels selects every existing slot; explicit factors match both dimensions after rounding up.
#[derive(Debug, Clone)]
#[must_use]
pub struct RefreshOverviews {
    options: Maintenance,
    bounds: Option<[f64; 4]>,
}
impl RefreshOverviews {
    /// Describe the existing target to maintain.
    pub fn new(target: impl Into<Source>) -> Self {
        Self {
            options: Maintenance::new(target),
            bounds: None,
        }
    }
    common!();
    sampling!();
    /// Refresh only pixels intersecting this source-CRS rectangle (GDAL 3.12+).
    /// Partial windows use sparse temporary storage under CPL_TMPDIR or the system temp directory.
    pub fn region(mut self, bounds: [f64; 4]) -> Self {
        self.bounds = Some(bounds);
        self
    }
    /// Maintain synchronously.
    pub fn run(self) -> Result<Report> {
        self.run_with(&mut Control::default())
    }
    /// Maintain with borrowed progress/cancellation; failure does not roll back.
    pub fn run_with(self, control: &mut Control<'_>) -> Result<Report> {
        self.options.run(Action::Refresh(self.bounds), control)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[cfg(gdal_3_12)]
    fn regional_refresh_preserves_alpha_masks_and_palette_colors() -> anyhow::Result<()> {
        use gdal::raster::{
            Buffer, ColorEntry, ColorInterpretation as Color, ColorTable, PaletteInterpretation,
        };
        let dir = tempfile::tempdir()?;
        let (w, h) = (55, 37);
        for storage in [OverviewStorage::Internal, OverviewStorage::External] {
            for palette in [false, true] {
                for method in [
                    Resampling::Average,
                    Resampling::Bilinear,
                    Resampling::Mode,
                    Resampling::Rms,
                ] {
                    let path = dir
                        .path()
                        .join(format!("{storage:?}-{palette}-{method:?}.tif"));
                    let native = dir
                        .path()
                        .join(format!("ref-{storage:?}-{palette}-{method:?}.tif"));
                    let bands = if palette { 1 } else { 4 };
                    let create = |p: &Path, changed| -> anyhow::Result<()> {
                        let mut ds = gdal::DriverManager::get_driver_by_name("GTiff")?
                            .create_with_band_type::<u8, _>(p, w, h, bands)?;
                        ds.set_geo_transform(&[0., 1., 0., h as f64, 0., -1.])?;
                        if palette {
                            let mut table = ColorTable::new(PaletteInterpretation::Rgba);
                            for (i, (r, g, b)) in
                                [(0, 0, 255), (0, 255, 2), (255, 0, 4), (123, 96, 5)]
                                    .into_iter()
                                    .enumerate()
                            {
                                table.set_color_entry(i as u16, &ColorEntry::rgba(r, g, b, 255));
                            }
                            ds.rasterband(1)?.set_color_table(&table);
                        } else {
                            for (i, color) in [
                                Color::RedBand,
                                Color::GreenBand,
                                Color::BlueBand,
                                Color::AlphaBand,
                            ]
                            .into_iter()
                            .enumerate()
                            {
                                ds.rasterband(i + 1)?.set_color_interpretation(color)?;
                            }
                        }
                        for n in 1..=bands {
                            if changed {
                                let pixels = (0..w * h)
                                    .map(|i| {
                                        let (x, y) = (i % w, i / w);
                                        if palette {
                                            ((x * 13 + y * 7 + (x * y) % 17) % 4) as u8
                                        } else if n == 4 {
                                            if (x + y) % 3 == 0 { 0 } else { 255 }
                                        } else {
                                            ((x * 17 + y * 11 + n * 13) % 201 + 1) as u8
                                        }
                                    })
                                    .collect();
                                ds.rasterband(n)?.write(
                                    (0, 0),
                                    (w, h),
                                    &mut Buffer::new((w, h), pixels),
                                )?;
                            } else {
                                ds.rasterband(n)?
                                    .fill(if n == 4 { 255. } else { 2. }, None)?;
                            }
                        }
                        ds.close()?;
                        Ok(())
                    };
                    create(&path, false)?;
                    create(&native, true)?;
                    let _ = BuildOverviews::new(&path)
                        .levels([3, 5])
                        .storage(storage)
                        .run()?;
                    let source = Dataset::open(&native)?;
                    let update = Source::new(&path).open_update(Kind::Raster)?;
                    for n in 1..=bands {
                        update.rasterband(n)?.write(
                            (0, 0),
                            (w, h),
                            &mut source.rasterband(n)?.read_band_as::<u8>()?,
                        )?;
                    }
                    update.close()?;
                    source.close()?;
                    let mut d = if storage == OverviewStorage::External {
                        Dataset::open(&native)?
                    } else {
                        Source::new(&native).open_update(Kind::Raster)?
                    };
                    d.build_overviews(resampling(method), &[5], &[])?;
                    d.close()?;
                    let _ = RefreshOverviews::new(&path)
                        .levels([5])
                        .region([13., 9., 39., 29.])
                        .resampling(method)
                        .storage(storage)
                        .run()?;
                    let (observed, reference) = (Dataset::open(&path)?, Dataset::open(&native)?);
                    for n in 1..=bands {
                        let old = if n == 4 { 255 } else { 2 };
                        let band = observed.rasterband(n)?;
                        assert_eq!(band.overview_count()?, 2);
                        assert!(
                            band.overview(0)?
                                .read_band_as::<u8>()?
                                .data()
                                .iter()
                                .all(|&v| v == old)
                        );
                        let actual = band.overview(1)?.read_band_as::<u8>()?;
                        let correct = reference.rasterband(n)?.overview(0)?.read_band_as::<u8>()?;
                        let (dw, dh) = actual.shape();
                        for i in 0..dw * dh {
                            let inside = (13 * dw / w..(39 * dw).div_ceil(w)).contains(&(i % dw))
                                && (8 * dh / h..(28 * dh).div_ceil(h)).contains(&(i / dw));
                            assert_eq!(
                                actual.data()[i],
                                if inside { correct.data()[i] } else { old },
                                "{storage:?} palette={palette} {method:?} band {n} pixel {i}"
                            );
                        }
                    }
                }
            }
        }
        Ok(())
    }

    #[test]
    fn internal_refresh_refuses_custom_external_overview_storage() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let base = dir.path().join("base.tif");
        let custom = dir.path().join("custom-overview.tif");
        super::super::test_support::fixture(&base, 8, 8, &[3u8; 64])?;
        super::super::test_support::fixture(&custom, 4, 4, &[7u8; 16])?;
        let mut ds = Source::new(&base).open_update(Kind::Raster)?;
        ds.set_metadata_item("OVERVIEW_FILE", custom.to_str().unwrap(), "OVERVIEWS")?;
        ds.close()?;
        assert_eq!(Dataset::open(&base)?.rasterband(1)?.overview_count()?, 1);
        let before = (std::fs::read(&base)?, std::fs::read(&custom)?);
        let error = RefreshOverviews::new(&base)
            .storage(OverviewStorage::Internal)
            .run()
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported);
        assert_eq!(error.output_state(), OutputState::NotModified);
        assert_eq!(before, (std::fs::read(&base)?, std::fs::read(&custom)?));
        Ok(())
    }

    #[test]
    fn refresh_refuses_an_unrelated_vrt_overview_before_opening_for_update() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let vrt = dir.path().join("base.vrt");
        let victim = dir.path().join("referenced.tif");
        let owned = sidecar(&vrt, ".ovr");
        super::super::test_support::fixture(&victim, 4, 4, &[7u8; 16])?;
        super::super::test_support::fixture(&owned, 4, 4, &[3u8; 16])?;
        std::fs::write(
            &vrt,
            r#"<VRTDataset rasterXSize="8" rasterYSize="8"><VRTRasterBand dataType="Byte" band="1"><Overview><SourceFilename relativeToVRT="1">referenced.tif</SourceFilename><SourceBand>1</SourceBand></Overview></VRTRasterBand></VRTDataset>"#,
        )?;
        let original = (std::fs::read(&victim)?, std::fs::read(&owned)?);
        let error = RefreshOverviews::new(&vrt).run().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported);
        assert_eq!(error.output_state(), OutputState::NotModified);
        assert_eq!(original, (std::fs::read(&victim)?, std::fs::read(&owned)?));
        Ok(())
    }

    #[test]
    #[cfg(gdal_3_12)]
    fn refresh_updates_only_requested_mask_and_data_slots() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        for (storage, internal_mask) in [
            (OverviewStorage::Internal, "YES"),
            (OverviewStorage::Internal, "NO"),
            (OverviewStorage::External, "NO"),
            (OverviewStorage::External, "YES"),
        ] {
            let config = GdalOptions::default().set("GDAL_TIFF_INTERNAL_MASK", internal_mask)?;
            let _scope = config.enter();
            let path = dir.path().join(format!("{storage:?}-{internal_mask}.tif"));
            let mut ds = gdal::DriverManager::get_driver_by_name("GTiff")?
                .create_with_band_type::<u8, _>(&path, 16, 16, 2)?;
            ds.set_geo_transform(&[0., 1., 0., 16., 0., -1.])?;
            for n in 1..=2 {
                ds.rasterband(n)?.fill(n as f64, None)?;
            }
            ds.rasterband(1)?.create_mask_band(true)?;
            ds.rasterband(1)?.open_mask_band()?.fill(255., None)?;
            ds.close()?;
            let _ = BuildOverviews::new(&path)
                .levels([2, 4])
                .storage(storage)
                .run()?;
            let ds = Source::new(&path).open_update(Kind::Raster)?;
            for n in 1..=2 {
                ds.rasterband(n)?.fill((n + 10) as f64, None)?;
            }
            ds.rasterband(1)?.open_mask_band()?.fill(0., None)?;
            ds.close()?;
            let _ = RefreshOverviews::new(&path)
                .levels([4])
                .storage(storage)
                .region([0., 8., 8., 16.])
                .run()?;
            let ds = Dataset::open(&path)?;
            for n in 1..=2 {
                let band = ds.rasterband(n)?;
                assert_eq!(band.overview_count()?, 2);
                let untouched = band.overview(0)?;
                let updated = band.overview(1)?;
                assert!(
                    untouched
                        .read_band_as::<u8>()?
                        .data()
                        .iter()
                        .all(|&v| v == n as u8)
                );
                let values = updated.read_band_as::<u8>()?;
                assert_eq!(values.data()[0], (n + 10) as u8);
                assert_eq!(values.data()[15], n as u8);
                let stored_mask = band.open_mask_band()?.overview_count()? != 0;
                assert_eq!(
                    stored_mask,
                    storage != OverviewStorage::External || internal_mask != "YES"
                );
                if stored_mask {
                    let mask = band.open_mask_band()?.overview(1)?.read_band_as::<u8>()?;
                    assert_eq!(mask.data()[0], 0, "{storage:?}/{internal_mask}/band{n}");
                    assert_eq!(mask.data()[15], 255);
                    if !updated.mask_flags()?.is_all_valid() {
                        assert_eq!(
                            updated.open_mask_band()?.read_band_as::<u8>()?.data(),
                            mask.data()
                        );
                    }
                    assert!(
                        band.open_mask_band()?
                            .overview(0)?
                            .read_band_as::<u8>()?
                            .data()
                            .iter()
                            .all(|&v| v == 255)
                    );
                }
            }
            ds.close()?;
        }
        Ok(())
    }

    #[test]
    #[cfg(gdal_3_12)]
    fn regional_native_sampling_matches_full_reference_on_fractional_grids() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let cfg = GdalOptions::default().set("CPL_TMPDIR", dir.path().to_str().unwrap())?;
        let (w, h) = (55usize, 37usize);
        for method in [
            Resampling::Nearest,
            Resampling::Average,
            Resampling::Bilinear,
            Resampling::Cubic,
            Resampling::CubicSpline,
            Resampling::Lanczos,
            Resampling::Mode,
            Resampling::Rms,
        ] {
            let path = dir.path().join(format!("partial-{method:?}.tif"));
            let full = dir.path().join(format!("full-{method:?}.tif"));
            super::super::test_support::fixture(&path, w, h, &vec![0u8; w * h])?;
            let _ = BuildOverviews::new(&path).levels([3]).run()?;
            let pixels = (0..w * h)
                .map(|i| ((i % w * 17 + i / w * 31) % 202 + 1) as u8)
                .collect::<Vec<_>>();
            super::super::test_support::fixture(&full, w, h, &pixels)?;
            let _ = BuildOverviews::new(&full)
                .levels([3])
                .resampling(method)
                .run()?;
            let ds = Source::new(&path).open_update(Kind::Raster)?;
            ds.rasterband(1)?.write(
                (0, 0),
                (w, h),
                &mut gdal::raster::Buffer::new((w, h), pixels),
            )?;
            ds.close()?;
            let _ = RefreshOverviews::new(&path)
                .levels([3])
                .resampling(method)
                .gdal_options(cfg.clone())
                .region([8., 14., 32., 32.])
                .run()?;
            let ds = Dataset::open(&path)?;
            let reference = Dataset::open(&full)?;
            let values = ds.rasterband(1)?.overview(0)?.read_band_as::<u8>()?;
            let full = reference.rasterband(1)?.overview(0)?.read_band_as::<u8>()?;
            let (dw, dh) = values.shape();
            for y in 0..dh {
                for x in 0..dw {
                    let inside = (8 * dw / w..(32 * dw).div_ceil(w)).contains(&x)
                        && (5 * dh / h..(23 * dh).div_ceil(h)).contains(&y);
                    assert_eq!(
                        values.data()[y * dw + x],
                        if inside { full.data()[y * dw + x] } else { 0 },
                        "{method:?}/{x}/{y}"
                    );
                }
            }
        }
        assert!(!std::fs::read_dir(dir.path())?.any(|f| f.is_ok_and(|f| f.path().is_dir())));
        Ok(())
    }

    #[test]
    #[cfg(gdal_3_12)]
    fn regional_refresh_does_not_read_a_distant_missing_vrt_tile() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let valid = dir.path().join("valid.tif");
        super::super::test_support::fixture(
            &valid,
            128,
            128,
            &(0..128 * 128).map(|i| (i % 4) as u8).collect::<Vec<_>>(),
        )?;
        let tile = |file, off| {
            format!(
                r#"<SimpleSource><SourceFilename relativeToVRT="1">{file}</SourceFilename><SourceBand>1</SourceBand><SourceProperties RasterXSize="128" RasterYSize="128" DataType="Byte" BlockXSize="128" BlockYSize="64"/><SrcRect xOff="0" yOff="0" xSize="128" ySize="128"/><DstRect xOff="{off}" yOff="{off}" xSize="128" ySize="128"/></SimpleSource>"#
            )
        };
        for palette in [false, true] {
            let path = dir.path().join(format!("two-tiles-{palette}.vrt"));
            let table = if palette {
                r#"<ColorInterp>Palette</ColorInterp><ColorTable><Entry c1="0" c2="0" c3="255" c4="255"/><Entry c1="0" c2="255" c3="2" c4="255"/><Entry c1="255" c2="0" c3="4" c4="255"/><Entry c1="123" c2="96" c3="5" c4="255"/></ColorTable>"#
            } else {
                ""
            };
            std::fs::write(
                &path,
                format!(
                    r#"<VRTDataset rasterXSize="8192" rasterYSize="8192"><GeoTransform>0,1,0,8192,0,-1</GeoTransform><VRTRasterBand dataType="Byte" band="1">{table}{}{}</VRTRasterBand></VRTDataset>"#,
                    tile("valid.tif", 0),
                    tile("deliberately-absent.tif", 8064)
                ),
            )?;
            let ds = gdal::DriverManager::get_driver_by_name("GTiff")?
                .create_with_band_type::<u8, _>(sidecar(&path, ".ovr"), 2048, 2048, 1)?;
            ds.close()?;
            let _ = RefreshOverviews::new(&path)
                .resampling(Resampling::Average)
                .region([0., 8128., 64., 8192.])
                .run()?;
            let d = Dataset::open(&path)?;
            let values =
                d.rasterband(1)?
                    .overview(0)?
                    .read_as::<u8>((0, 0), (32, 1), (32, 1), None)?;
            assert_eq!(values.data()[0], if palette { 3 } else { 2 });
            assert_eq!(values.data()[31], 0);
        }
        Ok(())
    }

    #[test]
    fn refresh_keeps_exact_slots_and_updates_pixels() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        for storage in [OverviewStorage::Internal, OverviewStorage::External] {
            for (w, h, level) in [
                (5usize, 10usize, 3u32),
                (5, 10, 4),
                (5, 17, 4),
                (1, 17, 3),
                (17, 1, 3),
            ] {
                let path = dir.path().join(format!("{storage:?}-{w}-{h}-{level}.tif"));
                super::super::test_support::fixture(&path, w, h, &vec![1u8; w * h])?;
                let _ = BuildOverviews::new(&path)
                    .storage(storage)
                    .levels([level])
                    .run()?;
                let size = (w.div_ceil(level as usize), h.div_ceil(level as usize));
                for (iteration, region) in [false, false, true, true].into_iter().enumerate() {
                    if region && !cfg!(gdal_3_12) {
                        continue;
                    }
                    let ds = Source::new(&path).open_update(Kind::Raster)?;
                    let new = iteration as u8 + 2;
                    ds.rasterband(1)?.fill(f64::from(new), None)?;
                    ds.close()?;
                    let mut refresh = RefreshOverviews::new(&path).storage(storage);
                    if iteration % 2 == 1 {
                        refresh = refresh.levels([level]);
                    }
                    if region {
                        refresh =
                            refresh.region([0., (h / 2) as f64, (w.div_ceil(2)) as f64, h as f64]);
                    }
                    let _ = refresh.run()?;
                    let ds = gdal::Dataset::open(&path)?;
                    let band = ds.rasterband(1)?;
                    assert_eq!(band.overview_count()?, 1, "{path:?}, iteration {iteration}");
                    let overview = band.overview(0)?;
                    assert_eq!(overview.size(), size);
                    let values = overview.read_band_as::<u8>()?;
                    assert_eq!(values.data()[0], new);
                    assert_eq!(values.data().last(), Some(if region { &3 } else { &new }));
                    ds.close()?;
                }
                let before = std::fs::read(if storage == OverviewStorage::Internal {
                    path.clone()
                } else {
                    sidecar(&path, ".ovr")
                })?;
                let error = RefreshOverviews::new(&path)
                    .storage(storage)
                    .levels([2])
                    .run()
                    .unwrap_err();
                assert_eq!(error.output_state(), OutputState::NotModified);
                assert_eq!(
                    before,
                    std::fs::read(if storage == OverviewStorage::Internal {
                        path
                    } else {
                        sidecar(&path, ".ovr")
                    })?
                );
            }
        }
        Ok(())
    }
    #[test]
    fn maintenance_refuses_overview_views_and_reports_partial_cancellation() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("base.tif");
        super::super::test_support::fixture(&path, 64, 64, &vec![1u8; 4096])?;
        let before = std::fs::read(&path)?;
        let error = BuildOverviews::new(Source::new(&path).open_option("OVERVIEW_LEVEL", "0"))
            .levels([2])
            .run()
            .unwrap_err();
        assert_eq!(error.output_state(), OutputState::NotModified);
        let mut calls = 0;
        let mut callback = |_: &crate::operation::Progress| {
            calls += 1;
            if calls > 1 {
                std::ops::ControlFlow::Break(())
            } else {
                std::ops::ControlFlow::Continue(())
            }
        };
        let error = BuildOverviews::new(&path)
            .levels([2, 4])
            .run_with(&mut Control::default().on_progress(&mut callback))
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Cancelled);
        assert_eq!(error.output_state(), OutputState::MayHaveChanged);
        assert_eq!(std::fs::read(&path)?, before);
        let _ = BuildOverviews::new(&path).levels([2, 4]).run()?;
        assert_eq!(
            super::super::Info::new(&path).run()?.value().bands[0]
                .overviews
                .len(),
            2
        );
        Ok(())
    }
    #[test]
    #[cfg(gdal_3_12)]
    fn regional_refresh_only_updates_requested_pixels_and_cog_is_protected() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("regional.tif");
        super::super::test_support::fixture(&path, 16, 16, &[1u8; 256])?;
        let _ = BuildOverviews::new(&path)
            .levels([2])
            .resampling(Resampling::Average)
            .run()?;
        let ds = gdal::Dataset::open_ex(
            &path,
            gdal::DatasetOptions {
                open_flags: gdal::GdalOpenFlags::GDAL_OF_UPDATE,
                ..Default::default()
            },
        )?;
        ds.rasterband(1)?.write(
            (0, 0),
            (2, 2),
            &mut gdal::raster::Buffer::new((2, 2), vec![9u8; 4]),
        )?;
        ds.rasterband(1)?.write(
            (14, 14),
            (2, 2),
            &mut gdal::raster::Buffer::new((2, 2), vec![13u8; 4]),
        )?;
        ds.close()?;
        let _ = RefreshOverviews::new(&path)
            .region([0., 12., 4., 16.])
            .resampling(Resampling::Average)
            .run()?;
        let ds = gdal::Dataset::open(&path)?;
        let values = ds.rasterband(1)?.overview(0)?.read_band_as::<u8>()?;
        assert_eq!(values.data()[0], 9);
        assert_eq!(values.data()[63], 1);
        ds.close()?;
        let cog = dir.path().join("cog.tif");
        let _ = super::super::Translate::new(&path, &cog)
            .driver("COG")
            .run()?;
        let before = std::fs::read(&cog)?;
        let error = BuildOverviews::new(&cog)
            .storage(OverviewStorage::Internal)
            .levels([2])
            .run()
            .unwrap_err();
        assert_eq!(error.output_state(), OutputState::NotModified);
        assert_eq!(std::fs::read(&cog)?, before);
        Ok(())
    }
    #[test]
    fn external_and_internal_lifecycle_and_alias_protection() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        for storage in [OverviewStorage::External, OverviewStorage::Internal] {
            let path = dir.path().join(format!("{storage:?}.tif"));
            super::super::test_support::fixture(&path, 16, 16, &vec![7u8; 256])?;
            let before = std::fs::read(&path)?;
            let _ = BuildOverviews::new(&path)
                .storage(storage)
                .levels([2, 4])
                .resampling(Resampling::Average)
                .run()?;
            let result = super::super::Info::new(&path).run()?;
            assert_eq!(result.value().bands[0].overviews, [(8, 8), (4, 4)]);
            if storage == OverviewStorage::External {
                assert_eq!(std::fs::read(&path)?, before);
                assert!(sidecar(&path, ".ovr").exists());
            }
            let _ = RefreshOverviews::new(&path).storage(storage).run()?;
            let _ = ClearOverviews::new(&path).storage(storage).run()?;
            assert!(
                super::super::Info::new(&path).run()?.value().bands[0]
                    .overviews
                    .is_empty()
            );
        }
        let plain = dir.path().join("plain.tif");
        super::super::test_support::fixture(&plain, 16, 16, &[3u8; 256])?;
        let vrt = dir.path().join("plain.vrt");
        let _ = super::super::Translate::new(&plain, &vrt)
            .driver("VRT")
            .run()?;
        let before = std::fs::read(&plain)?;
        let _ = BuildOverviews::new(&vrt).levels([2, 4]).run()?;
        assert_eq!(
            super::super::Info::new(&vrt).run()?.value().bands[0].overviews,
            [(8, 8), (4, 4)]
        );
        let _ = ClearOverviews::new(&vrt).run()?;
        assert_eq!(std::fs::read(&plain)?, before);
        #[cfg(unix)]
        {
            let src = dir.path().join("External.tif");
            let alias = sidecar(&src, ".ovr");
            if alias.exists() {
                std::fs::remove_file(&alias)?;
            }
            std::os::unix::fs::symlink(&src, &alias)?;
            let error = BuildOverviews::new(&src).levels([2]).run().unwrap_err();
            assert_eq!(error.output_state(), OutputState::NotModified);
        }
        Ok(())
    }
}

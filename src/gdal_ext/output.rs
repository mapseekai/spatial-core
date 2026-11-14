use crate::{
    dataset::{Source, local_name},
    operation::{Error, ErrorKind, Result, Stage},
};
use gdal::{Dataset, Driver, Metadata};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

pub(crate) fn absolute_output(path: &Path) -> Result<PathBuf> {
    local_name(path)?;
    if path
        .to_str()
        .is_some_and(|v| v.contains("://") || v.starts_with("/vsi") || v.starts_with("PG:"))
    {
        return Err(Error::invalid(
            "typed output must be a local path; use a native operation for GDAL names",
        ));
    }
    let filename = path
        .file_name()
        .ok_or_else(|| Error::invalid("output must name a file or new dataset directory"))?;
    let parent = path
        .parent()
        .filter(|v| !v.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    Ok(parent.canonicalize().map_err(io_error)?.join(filename))
}

pub(crate) fn reject_alias(source: &Source, output: &Path) -> Result<()> {
    AliasCheck::new(output)?.check_source(source)
}

pub(crate) fn reject_dataset_alias(input: &Dataset, output: &Path) -> Result<()> {
    AliasCheck::new(output)?.check_dataset(input)
}

// Snapshots last only through one operation's preflight; callers own the output directory.
pub(crate) struct AliasCheck {
    output: PathBuf,
    targets: HashMap<PathBuf, FileIdentity>,
    inputs: HashMap<PathBuf, FileIdentity>,
}
impl AliasCheck {
    pub(crate) fn new(output: &Path) -> Result<Self> {
        let mut check = Self {
            output: output.to_owned(),
            targets: HashMap::new(),
            inputs: HashMap::new(),
        };
        check.add_driver(None)?;
        Ok(check)
    }

    pub(crate) fn add_driver(&mut self, driver: Option<&str>) -> Result<()> {
        for path in output_files(&self.output, driver)? {
            if self.targets.contains_key(&path) {
                continue;
            }
            let target = FileIdentity::target(&path)?;
            for input in self.inputs.values() {
                input.reject_alias(&target)?;
            }
            self.targets.insert(path, target);
        }
        Ok(())
    }

    pub(crate) fn check_source(&mut self, source: &Source) -> Result<()> {
        if let Some(path) = source.local_path() {
            self.check_path(path)?;
        }
        Ok(())
    }

    pub(crate) fn check_path(&mut self, path: &Path) -> Result<()> {
        if !self.inputs.contains_key(path) {
            let input = FileIdentity::existing(path)?;
            for target in self.targets.values() {
                input.reject_alias(target)?;
            }
            self.inputs.insert(path.to_owned(), input);
        }
        Ok(())
    }

    pub(crate) fn check_dataset(&mut self, input: &Dataset) -> Result<()> {
        struct Files(*mut *mut std::ffi::c_char);
        impl Drop for Files {
            fn drop(&mut self) {
                // SAFETY: GDALGetFileList transfers this list to the caller; null is accepted.
                unsafe { gdal_sys::CSLDestroy(self.0) };
            }
        }
        // SAFETY: input is live; the returned list is independently owned.
        let files = Files(unsafe { gdal_sys::GDALGetFileList(input.c_dataset()) });
        if files.0.is_null() {
            return Ok(());
        }
        let mut entry = files.0;
        // SAFETY: GDAL returns a null-terminated array of NUL-terminated filenames.
        while !unsafe { *entry }.is_null() {
            // SAFETY: the current non-null entry lives until Files is dropped.
            let name = unsafe { std::ffi::CStr::from_ptr(*entry) };
            #[cfg(unix)]
            let path = {
                use std::os::unix::ffi::OsStrExt;
                Path::new(std::ffi::OsStr::from_bytes(name.to_bytes()))
            };
            #[cfg(not(unix))]
            let path = Path::new(name.to_str().map_err(|e| {
                Error::unsupported("cannot inspect dataset filenames").with_source(e)
            })?);
            if path.exists() {
                self.check_path(path)?;
            }
            // SAFETY: advance within the null-terminated array, checking its terminator next.
            entry = unsafe { entry.add(1) };
        }
        Ok(())
    }
}

struct FileIdentity {
    path: PathBuf,
    metadata: Option<std::fs::Metadata>,
}
impl FileIdentity {
    fn existing(path: &Path) -> Result<Self> {
        let path = path.canonicalize().map_err(io_error)?;
        let metadata = Some(path.metadata().map_err(io_error)?);
        Ok(Self { path, metadata })
    }

    fn target(path: &Path) -> Result<Self> {
        if let Ok(resolved) = path.canonicalize() {
            let metadata = Some(resolved.metadata().map_err(io_error)?);
            return Ok(Self {
                path: resolved,
                metadata,
            });
        }
        let resolved = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .canonicalize()
            .ok()
            .and_then(|parent| path.file_name().map(|name| parent.join(name)));
        Ok(Self {
            path: resolved.unwrap_or_else(|| path.to_owned()),
            metadata: None,
        })
    }

    fn reject_alias(&self, target: &Self) -> Result<()> {
        if self.path == target.path
            || self.metadata.as_ref().is_some_and(|m| m.is_dir())
                && target.path.starts_with(&self.path)
            || target.metadata.as_ref().is_some_and(|m| m.is_dir())
                && self.path.starts_with(&target.path)
        {
            return Err(Error::invalid("input and output refer to the same dataset"));
        }
        #[cfg(unix)]
        if let (Some(a), Some(b)) = (&self.metadata, &target.metadata) {
            use std::os::unix::fs::MetadataExt;
            if a.dev() == b.dev() && a.ino() == b.ino() {
                return Err(Error::invalid(
                    "input and output are hard links to the same dataset",
                ));
            }
        }
        Ok(())
    }
}

fn output_files(output: &Path, driver: Option<&str>) -> Result<Vec<PathBuf>> {
    let mut targets = vec![output.to_owned()];
    targets.extend(raster_companions(output, driver));
    if output.extension().is_some_and(|extension| {
        ["shp", "dbf", "shx"]
            .iter()
            .any(|known| extension.eq_ignore_ascii_case(known))
    }) {
        targets.extend(shapefile_companions(output)?);
    } else if output
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("gpkg"))
    {
        for suffix in ["-wal", "-shm", "-journal"] {
            let mut path = output.as_os_str().to_owned();
            path.push(suffix);
            targets.push(path.into());
        }
    } else if output.is_dir() {
        for entry in std::fs::read_dir(output).map_err(io_error)? {
            targets.push(entry.map_err(io_error)?.path());
        }
    }
    Ok(targets)
}

pub(crate) fn shapefile_companions(path: &Path) -> Result<Vec<PathBuf>> {
    let Some(stem) = path.file_stem() else {
        return Ok(Vec::new());
    };
    let stem = stem.as_encoded_bytes();
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if !parent.is_dir() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    for entry in std::fs::read_dir(parent).map_err(io_error)? {
        let entry = entry.map_err(io_error)?;
        let name = entry.file_name();
        let bytes = name.as_encoded_bytes();
        if bytes.len() <= stem.len()
            || bytes[stem.len()] != b'.'
            || !bytes[..stem.len()].eq_ignore_ascii_case(stem)
        {
            continue;
        }
        let suffix = &bytes[stem.len() + 1..];
        if [
            "shp", "shx", "dbf", "prj", "cpg", "qix", "sbn", "sbx", "fbn", "fbx", "ain", "aih",
            "ixs", "mxs", "idm", "ind", "qpj", "shp.xml", "dbf.xml",
        ]
        .iter()
        .any(|known| suffix.eq_ignore_ascii_case(known.as_bytes()))
            || suffix.ends_with(b".atx")
        {
            files.push(entry.path());
        }
    }
    Ok(files)
}

fn io_error(error: std::io::Error) -> Error {
    Error::invalid("cannot inspect local output").with_source(error)
}

pub(crate) fn check_vector_creation(
    path: &Path,
    driver: &Driver,
    layer_count: usize,
) -> Result<()> {
    if std::fs::symlink_metadata(path).is_ok() {
        return Err(output_exists());
    }
    if matches!(driver.short_name().as_str(), "MEM" | "Memory") {
        return Err(Error::unsupported(
            "typed local outputs require a persistent driver",
        ));
    }
    if driver.metadata_item("DCAP_VECTOR", "").as_deref() != Some("YES")
        || !(driver.metadata_item("DCAP_CREATE", "").as_deref() == Some("YES")
            || driver
                .metadata_item("DCAP_VECTOR_TRANSLATE_FROM", "")
                .as_deref()
                == Some("YES"))
    {
        return Err(Error::unsupported(
            "selected driver cannot create vector output",
        ));
    }
    if layer_count > 1
        && driver
            .metadata_item("DCAP_MULTIPLE_VECTOR_LAYERS", "")
            .as_deref()
            != Some("YES")
    {
        return Err(Error::unsupported(
            "selected driver does not support multiple vector layers",
        ));
    }
    match driver.short_name().as_str() {
        "GPKG" => {
            for suffix in ["-wal", "-shm", "-journal"] {
                let mut companion = path.as_os_str().to_owned();
                companion.push(suffix);
                if std::fs::symlink_metadata(Path::new(&companion)).is_ok() {
                    return Err(output_exists());
                }
            }
        }
        "ESRI Shapefile" => {
            if path
                .extension()
                .is_some_and(|v| v.eq_ignore_ascii_case("shp") || v.eq_ignore_ascii_case("dbf"))
                && !shapefile_companions(path)?.is_empty()
            {
                return Err(output_exists());
            }
        }
        "GeoJSON" | "FlatGeobuf" | "Parquet" | "PGDUMP" => {}
        _ => {
            if std::fs::read_dir(path.parent().expect("absolute path"))
                .map_err(io_error)?
                .next()
                .is_some()
            {
                return Err(Error::unsupported(
                    "unverified output driver requires a caller-owned empty output directory",
                ));
            }
        }
    }
    Ok(())
}

fn output_exists() -> Error {
    Error::new(
        ErrorKind::OutputExists,
        Stage::Prepare,
        "output dataset or companion file already exists",
    )
}

pub(crate) fn check_raster_creation(path: &Path, driver: &Driver) -> Result<()> {
    if std::fs::symlink_metadata(path).is_ok() {
        return Err(output_exists());
    }
    if matches!(driver.short_name().as_str(), "MEM" | "Memory") {
        return Err(Error::unsupported(
            "typed local outputs require a persistent driver",
        ));
    }
    if driver.metadata_item("DCAP_RASTER", "").as_deref() != Some("YES")
        || !["DCAP_CREATE", "DCAP_CREATECOPY"]
            .iter()
            .any(|key| driver.metadata_item(key, "").as_deref() == Some("YES"))
    {
        return Err(Error::unsupported(
            "selected driver cannot create raster output",
        ));
    }
    if !["GTiff", "COG", "PNG", "JPEG", "VRT"].contains(&driver.short_name().as_str())
        && std::fs::read_dir(path.parent().expect("absolute output"))
            .map_err(io_error)?
            .next()
            .is_some()
    {
        return Err(Error::unsupported(
            "unverified raster driver requires a caller-owned empty output directory",
        ));
    }
    for companion in raster_companions(path, Some(&driver.short_name())) {
        if std::fs::symlink_metadata(companion).is_ok() {
            return Err(output_exists());
        }
    }
    Ok(())
}

fn raster_companions(path: &Path, driver: Option<&str>) -> Vec<PathBuf> {
    let mut result = Vec::new();
    for suffix in [".aux.xml", ".ovr", ".msk"] {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        result.push(PathBuf::from(name));
    }
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let format = driver.unwrap_or(match ext.as_str() {
        "tif" | "tiff" => "GTiff",
        "png" => "PNG",
        "jpg" | "jpeg" => "JPEG",
        _ => "",
    });
    let extensions: &[&str] = match format {
        "GTiff" | "COG" => &["tfw", "tifw", "tiffw", "wld", "RPB", "rpb", "xml"],
        "PNG" => &["pgw", "pngw", "wld"],
        "JPEG" => &["jgw", "jpgw", "wld"],
        _ => &[],
    };
    result.extend(extensions.iter().map(|ext| path.with_extension(ext)));
    if matches!(format, "GTiff" | "COG") {
        let mut name = path.with_extension("").into_os_string();
        name.push("_RPC.TXT");
        result.push(name.into());
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn added_driver_targets_check_all_previously_seen_sources() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let source = dir.path().join("source.tfw");
        std::fs::write(&source, "caller owned")?;
        let output = dir.path().join("source.unknown");
        let mut check = AliasCheck::new(&output)?;
        check.check_path(&source)?;
        assert_eq!(
            check.add_driver(Some("GTiff")).unwrap_err().kind(),
            ErrorKind::InvalidArgument
        );
        assert_eq!(std::fs::read_to_string(source)?, "caller owned");
        assert!(!output.exists());
        Ok(())
    }

    #[test]
    fn normalized_alias_checks_preserve_links_and_directory_boundaries() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let source = dir.path().join("输入.tif");
        std::fs::write(&source, "source")?;
        for output in [&source, dir.path()] {
            assert!(AliasCheck::new(output)?.check_path(&source).is_err());
        }
        let child = dir.path().join("new.tif");
        assert!(AliasCheck::new(&child)?.check_path(dir.path()).is_err());
        let other = tempfile::tempdir()?;
        AliasCheck::new(&other.path().join("output.tif"))?.check_path(dir.path())?;
        #[cfg(unix)]
        {
            let symlink = dir.path().join("symlink.tif");
            let hardlink = dir.path().join("hardlink.tif");
            std::os::unix::fs::symlink(&source, &symlink)?;
            std::fs::hard_link(&source, &hardlink)?;
            for output in [&symlink, &hardlink] {
                assert!(AliasCheck::new(output)?.check_path(&source).is_err());
            }
            let linked_dir = other.path().join("linked-directory");
            std::os::unix::fs::symlink(dir.path(), &linked_dir)?;
            assert!(
                AliasCheck::new(&linked_dir.join("new.tif"))?
                    .check_path(dir.path())
                    .is_err()
            );
            // An aliased companion must be protected even if the output does not yet exist.
            let output = other.path().join("image.unknown");
            std::fs::hard_link(&source, output.with_extension("tfw"))?;
            let mut check = AliasCheck::new(&output)?;
            check.check_path(&source)?;
            assert!(check.add_driver(Some("GTiff")).is_err());
        }
        Ok(())
    }
}

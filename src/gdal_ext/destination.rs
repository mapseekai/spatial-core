// Portions of this destination/ownership helper correspond to georust/gdal.
// Copyright (c) 2014 Alex Morega. Upstream MIT notice is preserved in
// third_party/georust-gdal/LICENSE.txt; scope and local changes are documented
// in docs/third-party-notices.md. This is not a whole-project license change.

use super::ffi::path_to_c_string;
use gdal::errors::{GdalError, Result};
use std::ffi::CString;
use std::path::{Path, PathBuf};

/// Local output path for the borrowed-dataset conversion helpers.
pub enum DatasetDestination {
    Path(CString),
}

impl TryFrom<&str> for DatasetDestination {
    type Error = GdalError;

    fn try_from(path: &str) -> Result<Self> {
        Self::path(path)
    }
}

impl TryFrom<&Path> for DatasetDestination {
    type Error = GdalError;

    fn try_from(path: &Path) -> Result<Self> {
        Self::path(path)
    }
}

impl TryFrom<PathBuf> for DatasetDestination {
    type Error = GdalError;

    fn try_from(path: PathBuf) -> Result<Self> {
        Self::path(path)
    }
}

impl DatasetDestination {
    pub fn path<P: AsRef<Path>>(path: P) -> Result<Self> {
        let c_path = path_to_c_string(path.as_ref())?;
        Ok(Self::Path(c_path))
    }
}

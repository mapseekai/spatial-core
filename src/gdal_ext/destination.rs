// Portions of this destination/ownership helper correspond to georust/gdal.
// Copyright (c) 2014 Alex Morega. Upstream MIT notice is preserved in
// third_party/georust-gdal/LICENSE.txt; scope and local changes are documented
// in docs/third-party-notices.md. This is not a whole-project license change.

use super::ffi::path_to_c_string;
use gdal::Dataset;
use gdal::errors::{GdalError, Result};
use std::ffi::CString;
use std::mem::ManuallyDrop;
use std::path::{Path, PathBuf};

/// Path or Dataset to store Vectors or Raster
pub enum DatasetDestination {
    Path(CString),
    Dataset {
        dataset: ManuallyDrop<Dataset>,
        drop: bool,
    },
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

impl From<Dataset> for DatasetDestination {
    fn from(dataset: Dataset) -> Self {
        Self::dataset(dataset)
    }
}

impl Drop for DatasetDestination {
    fn drop(&mut self) {
        match self {
            Self::Path(_) => {}
            Self::Dataset { dataset, drop } => {
                if *drop {
                    // SAFETY: the ownership flag is cleared on transfer; otherwise this wrapper is the sole owner.
                    unsafe {
                        ManuallyDrop::drop(dataset);
                    }
                }
            }
        }
    }
}

impl DatasetDestination {
    pub fn dataset(dataset: Dataset) -> Self {
        Self::Dataset {
            dataset: ManuallyDrop::new(dataset),
            drop: true,
        }
    }

    pub fn path<P: AsRef<Path>>(path: P) -> Result<Self> {
        let c_path = path_to_c_string(path.as_ref())?;
        Ok(Self::Path(c_path))
    }

    /// Prevent the inner `Dataset` from being dropped when this
    /// `DatasetDestination` goes out of scope.
    ///
    /// # Safety
    ///
    /// The caller must ensure that ownership of the underlying GDAL dataset
    /// handle has been transferred to GDAL (e.g. via `GDALVectorTranslate` or
    /// `GDALTranslate`), so the handle will be freed by GDAL itself.
    pub unsafe fn relinquish_dataset_ownership(&mut self) {
        match self {
            Self::Path(_) => {}
            Self::Dataset { dataset: _, drop } => {
                *drop = false;
            }
        }
    }
}

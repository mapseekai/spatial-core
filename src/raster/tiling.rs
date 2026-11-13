//! Lazy raster windows, independent of database encoding and storage.
use crate::control::InvalidInput;

/// Window layout. Overlap changes stride, never source pixel values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TileLayout {
    /// One window covering the complete input.
    Whole,
    /// Fixed maximum tile dimensions, with smaller edge windows.
    Tiled {
        /// Maximum width in pixels.
        width: u32,
        /// Maximum height in pixels.
        height: u32,
        /// Shared pixels between adjacent windows on both axes.
        overlap: u32,
    },
}

impl Default for TileLayout {
    fn default() -> Self {
        Self::Tiled {
            width: 256,
            height: 256,
            overlap: 0,
        }
    }
}

impl TileLayout {
    /// Construct non-overlapping tiles.
    /// # Errors
    /// Rejects zero dimensions.
    pub fn tiled(width: u32, height: u32) -> Result<Self, InvalidInput> {
        let layout = Self::Tiled {
            width,
            height,
            overlap: 0,
        };
        layout.validate()?;
        Ok(layout)
    }
    /// Set the shared border width.
    /// # Errors
    /// Rejects overlap >= either dimension, and overlap on Whole.
    pub fn with_overlap(self, overlap: u32) -> Result<Self, InvalidInput> {
        let Self::Tiled { width, height, .. } = self else {
            return Err(InvalidInput("Whole has no tile overlap".into()));
        };
        let layout = Self::Tiled {
            width,
            height,
            overlap,
        };
        layout.validate()?;
        Ok(layout)
    }
    fn validate(self) -> Result<(), InvalidInput> {
        if let Self::Tiled {
            width,
            height,
            overlap,
        } = self
            && (width == 0 || height == 0 || overlap >= width || overlap >= height)
        {
            return Err(InvalidInput(
                "tile dimensions must be positive and greater than overlap".into(),
            ));
        }
        Ok(())
    }
}

/// Pixel window in source coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileWindow {
    /// Source x offset.
    pub x_off: usize,
    /// Source y offset.
    pub y_off: usize,
    /// Window width.
    pub width: usize,
    /// Window height.
    pub height: usize,
}

/// Constant-memory iterator, including edge windows and optional overlap.
#[derive(Debug, Clone)]
pub struct TileWindows {
    width: usize,
    height: usize,
    tile_width: usize,
    tile_height: usize,
    stride_x: usize,
    stride_y: usize,
    x: usize,
    y: usize,
    count: u64,
}

impl TileWindows {
    /// Plan windows without allocating a vector of tiles.
    /// # Errors
    /// Rejects invalid dimensions/overlap and tile-count overflow.
    pub fn new(width: usize, height: usize, layout: TileLayout) -> Result<Self, InvalidInput> {
        layout.validate()?;
        if width == 0 || height == 0 || width > isize::MAX as usize || height > isize::MAX as usize
        {
            return Err(InvalidInput(
                "source dimensions must be positive and fit native offsets".into(),
            ));
        }
        let (tile_width, tile_height, stride_x, stride_y) = match layout {
            TileLayout::Whole => (width, height, width, height),
            TileLayout::Tiled {
                width,
                height,
                overlap,
            } => (
                width as usize,
                height as usize,
                (width - overlap) as usize,
                (height - overlap) as usize,
            ),
        };
        let count = width
            .div_ceil(stride_x)
            .checked_mul(height.div_ceil(stride_y))
            .ok_or_else(|| InvalidInput("tile count overflow".into()))? as u64;
        Ok(Self {
            width,
            height,
            tile_width,
            tile_height,
            stride_x,
            stride_y,
            x: 0,
            y: 0,
            count,
        })
    }
    /// Total planned windows, independent of iteration progress.
    pub fn tile_count(&self) -> u64 {
        self.count
    }
}

impl Iterator for TileWindows {
    type Item = TileWindow;
    fn next(&mut self) -> Option<Self::Item> {
        if self.y >= self.height {
            return None;
        }
        let window = TileWindow {
            x_off: self.x,
            y_off: self.y,
            width: (self.width - self.x).min(self.tile_width),
            height: (self.height - self.y).min(self.tile_height),
        };
        self.x = self.x.saturating_add(self.stride_x);
        if self.x >= self.width {
            self.x = 0;
            self.y = self.y.saturating_add(self.stride_y);
        }
        Some(window)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rectangular_edges_overlap_and_large_lazy_layouts() -> anyhow::Result<()> {
        let windows = TileWindows::new(7, 5, TileLayout::tiled(4, 3)?.with_overlap(1)?)?;
        assert_eq!(windows.tile_count(), 9);
        let windows: Vec<_> = windows.collect();
        assert_eq!(windows[1].x_off, 3);
        assert_eq!(
            windows[8],
            TileWindow {
                x_off: 6,
                y_off: 4,
                width: 1,
                height: 1
            }
        );
        assert_eq!(
            TileWindows::new(1_000_000, 1_000_000, TileLayout::default())?
                .next()
                .unwrap()
                .width,
            256
        );
        assert!(TileLayout::tiled(0, 1).is_err());
        assert!(TileLayout::tiled(2, 2)?.with_overlap(2).is_err());
        Ok(())
    }
}

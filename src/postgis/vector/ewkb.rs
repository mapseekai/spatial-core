use super::{CoordinateDimension, GeometryColumn};
use crate::postgis::{PostGisError, Result};

struct Parser<'a> {
    input: &'a [u8],
    output: Vec<u8>,
}

impl Parser<'_> {
    fn take(&mut self, size: usize) -> Result<&[u8]> {
        if self.input.len() < size {
            return Err(invalid());
        }
        let (bytes, rest) = self.input.split_at(size);
        self.input = rest;
        Ok(bytes)
    }
    fn number(&mut self, little: bool) -> Result<u32> {
        let bytes: [u8; 4] = self.take(4)?.try_into().map_err(|_| invalid())?;
        Ok(if little {
            u32::from_le_bytes(bytes)
        } else {
            u32::from_be_bytes(bytes)
        })
    }
    fn geometry(
        &mut self,
        expected_kind: u32,
        dimension: CoordinateDimension,
        srid: Option<i32>,
    ) -> Result<()> {
        let endian = *self.take(1)?.first().ok_or_else(invalid)?;
        if endian > 1 {
            return Err(invalid());
        }
        let little = endian == 1;
        let encoded = self.number(little)?;
        if encoded & 0x10000000 != 0 {
            return Err(invalid());
        }
        let raw = encoded & 0x0fffffff;
        let iso = raw / 1000;
        let kind = raw % 1000;
        let flags = (encoded & 0xc0000000)
            | match iso {
                0 => 0,
                1 => 0x80000000,
                2 => 0x40000000,
                3 => 0xc0000000,
                _ => return Err(invalid()),
            };
        if kind != expected_kind || flags != dimension.flags() {
            return Err(invalid());
        }
        if encoded & 0x20000000 != 0 {
            self.number(little)?;
        }
        self.output.push(1);
        self.output.extend_from_slice(
            &(kind | flags | if srid.is_some() { 0x20000000 } else { 0 }).to_le_bytes(),
        );
        if let Some(srid) = srid {
            self.output.extend_from_slice(&srid.to_le_bytes());
        }
        match kind {
            1 => self.points(1, dimension, little)?,
            2 => {
                let count = self.number(little)?;
                self.output.extend_from_slice(&count.to_le_bytes());
                self.points(count, dimension, little)?;
            }
            3 => {
                let rings = self.number(little)?;
                self.output.extend_from_slice(&rings.to_le_bytes());
                if rings as usize > self.input.len() / 4 {
                    return Err(invalid());
                }
                for _ in 0..rings {
                    let count = self.number(little)?;
                    self.output.extend_from_slice(&count.to_le_bytes());
                    self.points(count, dimension, little)?;
                }
            }
            4..=6 => {
                let count = self.number(little)?;
                self.output.extend_from_slice(&count.to_le_bytes());
                if count as usize > self.input.len() / 5 {
                    return Err(invalid());
                }
                for _ in 0..count {
                    self.geometry(kind - 3, dimension, None)?;
                }
            }
            _ => return Err(invalid()),
        }
        Ok(())
    }
    fn points(&mut self, count: u32, dimension: CoordinateDimension, little: bool) -> Result<()> {
        let bytes = (count as usize)
            .checked_mul(dimension.axes())
            .and_then(|n| n.checked_mul(8))
            .ok_or_else(invalid)?;
        if self.input.len() < bytes {
            return Err(invalid());
        }
        for _ in 0..bytes / 8 {
            let mut value: [u8; 8] = self.take(8)?.try_into().map_err(|_| invalid())?;
            if !little {
                value.reverse();
            }
            self.output.extend_from_slice(&value);
        }
        Ok(())
    }
}

fn invalid() -> PostGisError {
    PostGisError::InvalidInput("WKB does not match geometry kind, dimensions or length".into())
}

pub(crate) fn encode(wkb: &[u8], column: &GeometryColumn) -> Result<String> {
    let mut parser = Parser {
        input: wkb,
        output: Vec::with_capacity(wkb.len() + 4),
    };
    parser.geometry(column.kind.number(), column.dimension, Some(column.srid))?;
    if !parser.input.is_empty() {
        return Err(invalid());
    }
    let mut hex = String::with_capacity(parser.output.len() * 2);
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    for byte in parser.output {
        hex.push(DIGITS[(byte >> 4) as usize] as char);
        hex.push(DIGITS[(byte & 15) as usize] as char);
    }
    Ok(hex)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn iso_multipoint_dimensions_and_lengths_are_checked() -> anyhow::Result<()> {
        for (wkt, dimension) in [
            ("MULTIPOINT ((1 2))", CoordinateDimension::Xy),
            ("MULTIPOINT Z ((1 2 3))", CoordinateDimension::Xyz),
            ("MULTIPOINT M ((1 2 3))", CoordinateDimension::Xym),
            ("MULTIPOINT ZM ((1 2 3 4))", CoordinateDimension::Xyzm),
        ] {
            let wkb = gdal::vector::Geometry::from_wkt(wkt)?.wkb()?;
            let column = GeometryColumn::new(
                "geom",
                super::super::GeometryKind::MultiPoint,
                dimension,
                4326,
            )?;
            assert!(encode(&wkb, &column).is_ok());
            assert!(encode(&wkb[..wkb.len() - 1], &column).is_err());
        }
        Ok(())
    }
}

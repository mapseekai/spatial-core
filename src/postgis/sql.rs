use super::{PostGisError, Result};

fn validate(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 63 || value.contains('\0') {
        return Err(PostGisError::InvalidInput(
            "SQL identifier must contain 1..=63 bytes without NUL".into(),
        ));
    }
    Ok(())
}

pub(crate) fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

/// A validated column identifier; dots are literal name characters.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ColumnName(String);

impl ColumnName {
    /// Validate an identifier without changing case or Unicode.
    /// # Errors
    /// Rejects empty, NUL-containing, or overlong names.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        validate(&value)?;
        Ok(Self(value))
    }
    /// Original identifier.
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub(crate) fn sql(&self) -> String {
        quote(&self.0)
    }
}

/// Schema and table are distinct validated identifiers.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TableName {
    schema: String,
    table: String,
}

impl TableName {
    /// Construct a qualified name. Neither component is split on dots.
    /// # Errors
    /// Rejects empty, NUL-containing, or overlong identifiers.
    pub fn new(schema: impl Into<String>, table: impl Into<String>) -> Result<Self> {
        let schema = schema.into();
        let table = table.into();
        validate(&schema)?;
        validate(&table)?;
        Ok(Self { schema, table })
    }
    /// Schema identifier.
    pub fn schema(&self) -> &str {
        &self.schema
    }
    /// Table identifier.
    pub fn table(&self) -> &str {
        &self.table
    }
    pub(crate) fn sql(&self) -> String {
        format!("{}.{}", quote(&self.schema), quote(&self.table))
    }

    pub(crate) fn lock_key(&self) -> i64 {
        // Stable FNV-1a, versioned namespace, NUL-separated validated components.
        let mut hash = 0xcbf29ce484222325u64;
        for byte in b"spatial-core:table:v1\0"
            .iter()
            .copied()
            .chain(self.schema.bytes())
            .chain([0])
            .chain(self.table.bytes())
        {
            hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
        }
        hash as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identifiers_are_quoted_without_changing_their_identity() -> Result<()> {
        assert_eq!(TableName::new("a.b", "x\"中")?.sql(), "\"a.b\".\"x\"\"中\"");
        assert!(ColumnName::new("a".repeat(64)).is_err());
        assert!(ColumnName::new("a\0b").is_err());
        assert_ne!(
            TableName::new("a", "b.c")?.lock_key(),
            TableName::new("a.b", "c")?.lock_key()
        );
        assert_ne!(
            TableName::new("A", "b")?.lock_key(),
            TableName::new("a", "b")?.lock_key()
        );
        Ok(())
    }
}

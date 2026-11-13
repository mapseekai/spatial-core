use crate::postgis::{ColumnName, PostGisError, Result};

/// PostgreSQL types supported by the exact vector input contract.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PostgresType {
    /// Signed 16-bit integer.
    SmallInt,
    /// Signed 32-bit integer.
    Integer,
    /// Signed 64-bit integer.
    BigInt,
    /// IEEE 32-bit float.
    Real,
    /// IEEE 64-bit float.
    DoublePrecision,
    /// Unconstrained exact decimal.
    Numeric,
    /// Exact decimal with PostgreSQL precision and signed scale.
    Decimal {
        /// Significant-digit limit (1..=1000).
        precision: u16,
        /// Decimal places, including negative scale.
        scale: i16,
    },
    /// Unbounded text.
    Text,
    /// Character-count limit; overlong input is rejected before COPY.
    Varchar(u32),
    /// Fixed-length character column.
    Char(u32),
    /// Boolean.
    Boolean,
    /// Calendar date.
    Date,
    /// Local timestamp with 0..=6 fractional digits.
    Timestamp(u8),
    /// Timestamp with explicit source offset and 0..=6 fractional digits.
    TimestampTz(u8),
    /// JSON text, including original whitespace and key order.
    Json,
    /// PostgreSQL's normalized JSON representation.
    Jsonb,
    /// One-dimensional int32 array, distinct from NULL.
    IntegerArray,
}

impl PostgresType {
    pub(crate) fn sql(&self) -> Result<String> {
        Ok(match self {
            Self::SmallInt => "smallint".into(),
            Self::Integer => "integer".into(),
            Self::BigInt => "bigint".into(),
            Self::Real => "real".into(),
            Self::DoublePrecision => "double precision".into(),
            Self::Numeric => "numeric".into(),
            Self::Decimal { precision, scale }
                if (1..=1000).contains(precision) && (-1000..=1000).contains(scale) =>
            {
                format!("numeric({precision},{scale})")
            }
            Self::Text => "text".into(),
            Self::Varchar(n) if (1..=10_485_760).contains(n) => format!("character varying({n})"),
            Self::Char(n) if (1..=10_485_760).contains(n) => format!("character({n})"),
            Self::Boolean => "boolean".into(),
            Self::Date => "date".into(),
            Self::Timestamp(p) if *p <= 6 => format!("timestamp({p}) without time zone"),
            Self::TimestampTz(p) if *p <= 6 => format!("timestamp({p}) with time zone"),
            Self::Json => "json".into(),
            Self::Jsonb => "jsonb".into(),
            Self::IntegerArray => "integer[]".into(),
            _ => {
                return Err(PostGisError::InvalidInput(
                    "invalid PostgreSQL type modifier".into(),
                ));
            }
        })
    }
}

/// One attribute column. Field order is the vector's order, never map iteration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnSchema {
    /// Validated destination name.
    pub name: ColumnName,
    /// Exact destination type.
    pub data_type: PostgresType,
    /// Whether missing/NULL input is accepted.
    pub nullable: bool,
}

impl ColumnSchema {
    /// Construct a nullable column. Set nullable=false to require every value.
    /// # Errors
    /// Rejects invalid names or type modifiers.
    pub fn new(name: impl Into<String>, data_type: PostgresType) -> Result<Self> {
        data_type.sql()?;
        Ok(Self {
            name: ColumnName::new(name)?,
            data_type,
            nullable: true,
        })
    }
}

/// Supported linear geometry families.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeometryKind {
    /// Point.
    Point,
    /// Line string.
    LineString,
    /// Polygon.
    Polygon,
    /// Collection of points.
    MultiPoint,
    /// Collection of line strings.
    MultiLineString,
    /// Collection of polygons.
    MultiPolygon,
}

impl GeometryKind {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Point => "Point",
            Self::LineString => "LineString",
            Self::Polygon => "Polygon",
            Self::MultiPoint => "MultiPoint",
            Self::MultiLineString => "MultiLineString",
            Self::MultiPolygon => "MultiPolygon",
        }
    }
    pub(crate) fn number(self) -> u32 {
        match self {
            Self::Point => 1,
            Self::LineString => 2,
            Self::Polygon => 3,
            Self::MultiPoint => 4,
            Self::MultiLineString => 5,
            Self::MultiPolygon => 6,
        }
    }
}

/// Spatial and measure dimensions are independent of geometry family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoordinateDimension {
    /// X/Y.
    Xy,
    /// X/Y/Z.
    Xyz,
    /// X/Y/measure.
    Xym,
    /// X/Y/Z/measure.
    Xyzm,
}

impl CoordinateDimension {
    pub(crate) fn suffix(self) -> &'static str {
        match self {
            Self::Xy => "",
            Self::Xyz => "Z",
            Self::Xym => "M",
            Self::Xyzm => "ZM",
        }
    }
    pub(crate) fn axes(self) -> usize {
        match self {
            Self::Xy => 2,
            Self::Xyz | Self::Xym => 3,
            Self::Xyzm => 4,
        }
    }
    pub(crate) fn flags(self) -> u32 {
        match self {
            Self::Xy => 0,
            Self::Xyz => 0x80000000,
            Self::Xym => 0x40000000,
            Self::Xyzm => 0xc0000000,
        }
    }
}

/// Destination geometry column, including its database SRID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeometryColumn {
    /// Destination name.
    pub name: ColumnName,
    /// Linear geometry family.
    pub kind: GeometryKind,
    /// Coordinate/measure dimensions.
    pub dimension: CoordinateDimension,
    /// Positive database SRID, initially an EPSG identifier.
    pub srid: i32,
    /// Allow NULL geometry. EMPTY geometry is distinct and allowed.
    pub nullable: bool,
}

impl GeometryColumn {
    /// Construct a non-null geometry column.
    /// # Errors
    /// Rejects invalid identifiers and non-positive/out-of-range SRIDs.
    pub fn new(
        name: impl Into<String>,
        kind: GeometryKind,
        dimension: CoordinateDimension,
        srid: i32,
    ) -> Result<Self> {
        if !(1..=998999).contains(&srid) {
            return Err(PostGisError::InvalidInput("SRID must be 1..=998999".into()));
        }
        Ok(Self {
            name: ColumnName::new(name)?,
            kind,
            dimension,
            srid,
            nullable: false,
        })
    }
}

/// Final destination contract; it also describes an explicitly allowed empty source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorSchema {
    /// Ordered attribute columns.
    pub columns: Vec<ColumnSchema>,
    /// Database-generated row identifier.
    pub identity: ColumnName,
    /// Geometry contract.
    pub geometry: GeometryColumn,
}

impl VectorSchema {
    /// Create a schema with an `_id` bigint identity column.
    /// # Errors
    /// Rejects duplicate/reserved destination names and invalid types.
    pub fn new(columns: Vec<ColumnSchema>, geometry: GeometryColumn) -> Result<Self> {
        let schema = Self {
            columns,
            identity: ColumnName::new("_id")?,
            geometry,
        };
        schema.validate()?;
        Ok(schema)
    }
    pub(crate) fn validate(&self) -> Result<()> {
        let mut names = std::collections::HashSet::new();
        names.insert(self.identity.as_str());
        if !names.insert(self.geometry.name.as_str()) {
            return Err(PostGisError::InvalidInput(
                "identity and geometry names collide".into(),
            ));
        }
        if !(1..=998999).contains(&self.geometry.srid) {
            return Err(PostGisError::InvalidInput("invalid geometry SRID".into()));
        }
        for column in &self.columns {
            column.data_type.sql()?;
            if !names.insert(column.name.as_str()) {
                return Err(PostGisError::InvalidInput(format!(
                    "duplicate destination column {}",
                    column.name.as_str()
                )));
            }
        }
        Ok(())
    }
}

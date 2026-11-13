//! Vector schemas, transactional import, and exact database metadata.
mod copy;
mod ewkb;
mod metadata;
mod schema;
mod source;
mod table;
mod writer;

pub use metadata::{DatabaseColumn, VectorMetadata, read_metadata};
pub use schema::{
    ColumnSchema, CoordinateDimension, GeometryColumn, GeometryKind, PostgresType, VectorSchema,
};
pub use source::VectorSource;
pub use writer::{PostGisVectorWriter, VectorWriteOptions, VectorWriteReport};

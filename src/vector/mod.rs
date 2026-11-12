//! ```compile_fail
//! use spatial_core::vector::MinioSettings;
//! ```

pub mod copy;
pub mod metadata;

pub use copy::{
    add_prefix_from_ext, copy_to_postgis, get_file_name, store_postgis, to_geoparquet,
    vector_to_pg, zipshp2sql,
};
pub use metadata::{VectorMetadata, compute_vector_metadata};

use super::{source::SourceInfo, writer::RasterWriteOptions};
use crate::postgis::{
    Result, TableName,
    connection::Capabilities,
    transaction::{FinalSql, TableColumn, TablePlan},
};

pub(super) fn plan(
    info: &SourceInfo,
    options: &RasterWriteOptions,
    table: &TableName,
    caps: &Capabilities,
) -> Result<TablePlan> {
    let raster_type = caps.raster_function("raster")?;
    let raster = info.schema.raster.sql();
    let columns = vec![
        TableColumn {
            name: info.schema.identity.as_str().into(),
            sql_type: "bigint".into(),
            nullable: false,
            identity: true,
        },
        TableColumn {
            name: info.schema.raster.as_str().into(),
            sql_type: raster_type,
            nullable: false,
            identity: false,
        },
    ];
    let hull = format!("{}({raster})", caps.raster_function("st_convexhull")?);
    let index = format!("CREATE INDEX ON {} USING gist ({hull})", table.sql());
    // Select only constraints that hold for this grid; NEVER infer regular blocking from tiling alone.
    let mut constraints = vec![
        "srid",
        "scale_x",
        "scale_y",
        "same_alignment",
        "num_bands",
        "pixel_types",
        "nodata_values",
        "out_db",
        "extent",
    ];
    if info.fixed_tiles {
        constraints.extend(["blocksize_x", "blocksize_y"]);
    }
    let names = constraints
        .iter()
        .map(|s| format!("'{s}'"))
        .collect::<Vec<_>>()
        .join(",");
    let add = caps.raster_function("addrasterconstraints")?;
    let constraints = FinalSql::Check {
        sql: format!(
            "SELECT {add}($1::text::name, $2::text::name, $3::text::name, VARIADIC ARRAY[{names}]::text[])"
        ),
        parameters: vec![
            table.schema().into(),
            table.table().into(),
            info.schema.raster.as_str().into(),
        ],
    };
    let pixel_types = format!(
        "{{{}}}",
        info.schema
            .bands
            .iter()
            .map(|b| b.pixel_type.name())
            .collect::<Vec<_>>()
            .join(",")
    );
    let nodata = format!(
        "{{{}}}",
        info.schema
            .bands
            .iter()
            .map(|b| b
                .nodata
                .map(|v| v.to_string())
                .unwrap_or_else(|| "NULL".into()))
            .collect::<Vec<_>>()
            .join(",")
    );
    let outdb = matches!(options.storage, super::RasterStorage::OutDb { .. });
    let outdb = format!(
        "{{{}}}",
        vec![outdb.to_string(); info.schema.bands.len()].join(",")
    );
    let catalog = caps.raster_function("raster_columns")?;
    let check = FinalSql::Check {
        sql: format!(
            "SELECT EXISTS(SELECT 1 FROM {catalog} WHERE r_table_schema=$1 AND r_table_name=$2 AND r_raster_column=$3 AND srid=$4::text::integer AND num_bands=$5::text::integer AND pixel_types=$6::text::text[] AND cardinality(nodata_values)=$5::text::integer AND out_db=$7::text::boolean[] AND scale_x IS NOT NULL AND scale_y IS NOT NULL AND same_alignment AND extent IS NOT NULL AND (NOT $8::text::boolean OR (blocksize_x=$9::text::integer AND blocksize_y=$10::text::integer)))"
        ),
        parameters: vec![
            table.schema().into(),
            table.table().into(),
            info.schema.raster.as_str().into(),
            info.schema.srid.to_string(),
            info.schema.bands.len().to_string(),
            pixel_types.clone(),
            outdb.clone(),
            info.fixed_tiles.to_string(),
            info.tile_size.0.to_string(),
            info.tile_size.1.to_string(),
        ],
    };
    // raster_columns rounds scale/NoData; validate full precision on the stored raster headers.
    let values = FinalSql::Check {
        sql: format!(
            "SELECT NOT EXISTS(SELECT 1 FROM {} WHERE {}({raster}) IS DISTINCT FROM $1::text::float8 OR {}({raster}) IS DISTINCT FROM $2::text::float8 OR ARRAY(SELECT {}({raster},b) FROM generate_series(1,{}({raster})) b) IS DISTINCT FROM $3::text::float8[] OR {}({raster}) IS DISTINCT FROM $4::text::integer OR {}({raster}) IS DISTINCT FROM $5::text::float8 OR {}({raster}) IS DISTINCT FROM $6::text::float8 OR ARRAY(SELECT {}({raster},b) FROM generate_series(1,{}({raster})) b) IS DISTINCT FROM $7::text::text[] OR ARRAY(SELECT {}({raster},b) IS NOT NULL FROM generate_series(1,{}({raster})) b) IS DISTINCT FROM $8::text::boolean[])",
            table.sql(),
            caps.raster_function("st_scalex")?,
            caps.raster_function("st_scaley")?,
            caps.raster_function("st_bandnodatavalue")?,
            caps.raster_function("st_numbands")?,
            caps.raster_function("st_srid")?,
            caps.raster_function("st_skewx")?,
            caps.raster_function("st_skewy")?,
            caps.raster_function("st_bandpixeltype")?,
            caps.raster_function("st_numbands")?,
            caps.raster_function("st_bandpath")?,
            caps.raster_function("st_numbands")?,
        ),
        parameters: vec![
            info.schema.geo_transform[1].to_string(),
            info.schema.geo_transform[5].to_string(),
            nodata,
            info.schema.srid.to_string(),
            info.schema.geo_transform[2].to_string(),
            info.schema.geo_transform[4].to_string(),
            pixel_types,
            outdb,
        ],
    };
    let extent_sql = super::metadata::extent_sql(table, &info.schema.raster, caps)?;
    Ok(TablePlan {
        columns,
        srid: info.schema.srid,
        allow_empty: false,
        total: Some(info.count),
        finalize: vec![FinalSql::Execute(index), constraints, check],
        verify: Some(values),
        extent_sql,
    })
}

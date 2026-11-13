use super::metadata;
use super::{source::SourceInfo, writer::VectorWriteOptions};
use crate::postgis::{EmptyInputPolicy, sql::quote};
use crate::postgis::{
    Result, TableName,
    connection::Capabilities,
    transaction::{FinalSql, TableColumn, TablePlan},
};

pub(super) fn plan(
    info: &SourceInfo,
    options: &VectorWriteOptions,
    table: &TableName,
    caps: &Capabilities,
) -> Result<TablePlan> {
    let schema = &info.schema;
    let mut columns = vec![TableColumn {
        name: schema.identity.as_str().into(),
        sql_type: "bigint".into(),
        nullable: false,
        identity: true,
    }];
    for column in &schema.columns {
        columns.push(TableColumn {
            name: column.name.as_str().into(),
            sql_type: column.data_type.sql()?,
            nullable: column.nullable,
            identity: false,
        });
    }
    let geom = &schema.geometry;
    columns.push(TableColumn {
        name: geom.name.as_str().into(),
        sql_type: format!(
            "{}.geometry({}{},{})",
            quote(&caps.geometry),
            geom.kind.name(),
            geom.dimension.suffix(),
            geom.srid
        ),
        nullable: geom.nullable,
        identity: false,
    });
    let mut finalize = Vec::new();
    if options.spatial_index {
        finalize.push(FinalSql::Execute(format!(
            "CREATE INDEX ON {} USING gist ({})",
            table.sql(),
            geom.name.sql()
        )));
    }
    if options.cursor_index {
        finalize.push(FinalSql::Execute(format!(
            "CREATE INDEX ON {} (({}::text COLLATE \"C\"))",
            table.sql(),
            schema.identity.sql()
        )));
    }
    Ok(TablePlan {
        columns,
        srid: geom.srid,
        allow_empty: options.empty == EmptyInputPolicy::Allow,
        total: info.total,
        finalize,
        verify: None,
        extent_sql: metadata::extent_sql(table, &geom.name, caps),
    })
}

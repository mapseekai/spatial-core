use std::{sync::mpsc, thread, time::Duration};

use ::duckdb::{
    Connection,
    arrow::{datatypes::SchemaRef, record_batch::RecordBatch},
    core::{LogicalTypeHandle, LogicalTypeId},
    params_from_iter,
};

use crate::control::Cancellation;

use super::{Error, Result, Session, Value};

/// Per-operation cancellation and output budgets. Limits reject excess output;
/// they do not bound DuckDB's intermediate allocations or a consumer's retention.
#[derive(Default)]
#[must_use]
pub struct Control<'a> {
    pub cancellation: Option<&'a Cancellation>,
    pub max_rows: Option<u64>,
    /// Cumulative Arrow buffer bytes for streaming; encoded file bytes for COPY.
    pub max_bytes: Option<u64>,
}

impl<'a> Control<'a> {
    /// Observe a shared cooperative cancellation token.
    pub fn cancellation(mut self, cancellation: &'a Cancellation) -> Self {
        self.cancellation = Some(cancellation);
        self
    }

    /// Cap the total number of output rows accepted from this operation.
    pub fn max_rows(mut self, limit: u64) -> Self {
        self.max_rows = Some(limit);
        self
    }

    /// Cap the cumulative Arrow buffer bytes or exported file bytes.
    pub fn max_bytes(mut self, limit: u64) -> Self {
        self.max_bytes = Some(limit);
        self
    }

    pub(crate) fn check(&self) -> Result<()> {
        if self.cancellation.is_some_and(Cancellation::is_cancelled) {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }
}

impl<'a> From<&'a Cancellation> for Control<'a> {
    fn from(cancellation: &'a Cancellation) -> Self {
        Self::default().cancellation(cancellation)
    }
}

#[derive(Debug, Clone)]
pub struct QueryReport {
    /// Available even when the result has zero rows.
    pub schema: SchemaRef,
    pub rows: u64,
    pub batches: u64,
    pub arrow_bytes: u64,
}

/// A parameterized, trusted SQL SELECT, borrowing one session exclusively.
/// Use DuckDB Spatial functions directly; SQL semantics and units are explicit.
#[must_use = "queries do not execute until consumed or exported"]
pub struct Query<'a> {
    pub(crate) session: &'a mut Session,
    pub(crate) sql: String,
    pub(crate) parameters: Vec<Value>,
}

impl Session {
    pub fn query(
        &mut self,
        sql: impl Into<String>,
        parameters: impl IntoIterator<Item = Value>,
    ) -> Query<'_> {
        Query {
            session: self,
            sql: sql.into(),
            parameters: parameters.into_iter().collect(),
        }
    }
}

impl Query<'_> {
    pub fn schema(&mut self, control: &Control<'_>) -> Result<SchemaRef> {
        controlled(&self.session.connection, control, || {
            schema(&self.session.connection, &self.sql, &self.parameters)
        })
    }

    /// Consume one native Arrow batch at a time, without materializing a Vec.
    /// A callback can already have consumed a prefix when an error is returned;
    /// only an Ok report means the complete query succeeded. Panics unwind in Rust.
    pub fn for_each_batch(
        self,
        control: &Control<'_>,
        mut consume: impl FnMut(RecordBatch) -> Result<()>,
    ) -> Result<QueryReport> {
        controlled(&self.session.connection, control, || {
            // Validate native types before Arrow can erase information such as TIMETZ offsets.
            schema(&self.session.connection, &self.sql, &self.parameters)?;
            let mut statement = self
                .session
                .connection
                .prepare(&format!(
                    "SELECT * FROM ({}) AS spatial_core_query",
                    self.sql
                ))
                .map_err(|e| Error::engine("prepare query", e))?;
            let stream = statement
                .stream_arrow(params_from_iter(self.parameters.iter()))
                .map_err(|e| Error::engine("execute query", e))?;
            let mut report = QueryReport {
                schema: stream.get_schema(),
                rows: 0,
                batches: 0,
                arrow_bytes: 0,
            };
            for batch in stream {
                control.check()?;
                let batch = batch.map_err(|e| Error::engine("fetch Arrow batch", e))?;
                report.rows = report
                    .rows
                    .checked_add(batch.num_rows() as u64)
                    .ok_or(Error::LimitExceeded("row counter"))?;
                report.arrow_bytes = report
                    .arrow_bytes
                    .checked_add(batch.get_array_memory_size() as u64)
                    .ok_or(Error::LimitExceeded("byte counter"))?;
                if control.max_rows.is_some_and(|limit| report.rows > limit) {
                    return Err(Error::LimitExceeded("row budget"));
                }
                if control
                    .max_bytes
                    .is_some_and(|limit| report.arrow_bytes > limit)
                {
                    return Err(Error::LimitExceeded("Arrow byte budget"));
                }
                consume(batch)?;
                report.batches += 1;
            }
            control.check()?;
            Ok(report)
        })
    }
}

pub(crate) fn schema(
    connection: &Connection,
    sql: &str,
    parameters: &[Value],
) -> Result<SchemaRef> {
    let mut statement = connection
        .prepare(&format!(
            "SELECT * FROM ({sql}) AS spatial_core_schema LIMIT 0"
        ))
        .map_err(|e| Error::engine("prepare schema", e))?;
    let schema = {
        let mut stream = statement
            .stream_arrow(params_from_iter(parameters.iter()))
            .map_err(|e| Error::engine("query schema", e))?;
        let schema = stream.get_schema();
        for batch in &mut stream {
            batch.map_err(|e| Error::engine("fetch schema", e))?;
        }
        schema
    };
    for index in 0..statement.column_count() {
        if contains_timetz(&statement.column_logical_type(index)) {
            return Err(Error::Unsupported(
                "TIMETZ loses its offset in Arrow; cast it explicitly before reading",
            ));
        }
    }
    Ok(schema)
}

fn contains_timetz(data_type: &LogicalTypeHandle) -> bool {
    data_type.id() == LogicalTypeId::TimeTZ
        || (0..data_type.num_children()).any(|index| contains_timetz(&data_type.child(index)))
}

pub(crate) fn controlled<T>(
    connection: &Connection,
    control: &Control<'_>,
    run: impl FnOnce() -> Result<T>,
) -> Result<T> {
    control.check()?;
    let Some(token) = control.cancellation else {
        return run();
    };
    let interrupt = connection.interrupt_handle();
    thread::scope(|scope| {
        // The sender is local to this scope, so unwinding also wakes the listener.
        let (stop, listener) = mpsc::sync_channel::<()>(0);
        let monitor = thread::Builder::new()
            .name("spatial-duckdb-cancel".into())
            .spawn_scoped(scope, move || {
                loop {
                    match listener.recv_timeout(Duration::from_millis(10)) {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            // Repeated interrupts cover cancellation just before a new
                            // native statement clears its interrupt flag.
                            if token.is_cancelled() {
                                interrupt.interrupt();
                            }
                        }
                    }
                }
            })?;
        let result = run();
        drop(stop);
        monitor
            .join()
            .map_err(|_| Error::InvalidInput("cancellation monitor panicked"))?;
        control.check()?;
        result
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::duckdb::test_session;

    #[test]
    #[ignore = "requires version-matched Spatial/httpfs extensions"]
    fn native_timetz_is_rejected_before_arrow_erases_offsets() {
        use crate::duckdb::arrow::array::{IntervalMonthDayNanoArray, Time64MicrosecondArray};
        let directory = tempfile::tempdir().unwrap();
        let mut session = test_session(directory.path());
        for sql in [
            "SELECT '12:34:56.123456+05:30'::TIMETZ AS time",
            "SELECT {'times': ['12:34:56.123456+05:30'::TIMETZ]} AS attributes",
            "SELECT map(['time'], ['12:34:56.123456+05:30'::TIMETZ]) AS times",
            "SELECT union_value(time := '12:34:56.123456+05:30'::TIMETZ) AS time",
        ] {
            assert!(matches!(
                session.query(sql, []).schema(&Control::default()),
                Err(Error::Unsupported(_))
            ));
            assert!(matches!(
                session
                    .query(sql, [])
                    .for_each_batch(&Control::default(), |_| panic!("TIMETZ escaped validation")),
                Err(Error::Unsupported(_))
            ));
        }
        assert!(matches!(
            session
                .query(
                    "SELECT ?::TIMETZ WHERE false",
                    [Value::Text("12:00:00+05:30".into())]
                )
                .schema(&Control::default()),
            Err(Error::Unsupported(_))
        ));
        // Ordinary TIME and Arrow INTERVAL retain their precision and remain supported.
        session
            .query(
                "SELECT '12:34:56.123456'::TIME, INTERVAL '1 microsecond'",
                [],
            )
            .for_each_batch(&Control::default(), |batch| {
                assert_eq!(
                    batch
                        .column(0)
                        .as_any()
                        .downcast_ref::<Time64MicrosecondArray>()
                        .unwrap()
                        .value(0),
                    45_296_123_456
                );
                assert_eq!(
                    batch
                        .column(1)
                        .as_any()
                        .downcast_ref::<IntervalMonthDayNanoArray>()
                        .unwrap()
                        .value(0)
                        .nanoseconds,
                    1000
                );
                Ok(())
            })
            .unwrap();
        session.close().unwrap();
    }

    #[test]
    #[ignore = "requires version-matched Spatial/httpfs extensions"]
    fn cancellation_interrupts_execution_and_does_not_reach_next_query() {
        let directory = tempfile::tempdir().unwrap();
        let mut session = test_session(directory.path());
        let token = Cancellation::default();
        let control = Control {
            cancellation: Some(&token),
            ..Control::default()
        };
        thread::scope(|scope| {
            scope.spawn(|| {
                thread::sleep(Duration::from_millis(50));
                token.cancel();
            });
            let started = std::time::Instant::now();
            let result = session
                .query("SELECT sum(sin(i)) FROM range(10000000000) t(i)", [])
                .for_each_batch(&control, |_| Ok(()));
            assert!(matches!(result, Err(Error::Cancelled)), "{result:?}");
            assert!(started.elapsed() < Duration::from_secs(10));
        });
        assert!(matches!(
            session
                .query("SELECT 1", [])
                .for_each_batch(&control, |_| Ok(())),
            Err(Error::Cancelled)
        ));
        let report = session
            .query("SELECT 42", [])
            .for_each_batch(&Control::default(), |_| Ok(()))
            .unwrap();
        assert_eq!(report.rows, 1);
        session.close().unwrap();
    }

    #[test]
    #[ignore = "requires version-matched Spatial/httpfs extensions"]
    fn fetch_failure_callback_failure_and_limits_never_report_success() {
        let directory = tempfile::tempdir().unwrap();
        let mut session = test_session(directory.path());
        let control = Control::default();
        let mut seen = 0;
        let result = session.query("SELECT CASE WHEN i < 1000000 THEN i ELSE error('late fixture error') END FROM range(2000000) t(i)", [])
            .for_each_batch(&control, |batch| { seen += batch.num_rows(); Ok(()) });
        assert!(matches!(result, Err(Error::Engine { .. })));
        let interrupt = session.connection.interrupt_handle();
        seen = 0;
        let result = session
            .query("SELECT i FROM range(100000000) t(i)", [])
            .for_each_batch(&control, |batch| {
                seen += batch.num_rows();
                interrupt.interrupt();
                Ok(())
            });
        assert!(
            seen > 0,
            "test must exercise failure after an emitted prefix"
        );
        assert!(
            matches!(
                result,
                Err(Error::Engine {
                    stage: "fetch Arrow batch",
                    ..
                })
            ),
            "{result:?}"
        );
        let result = session
            .query("SELECT * FROM range(10000)", [])
            .for_each_batch(&control, |_| Err(Error::InvalidInput("consumer stopped")));
        assert!(matches!(
            result,
            Err(Error::InvalidInput("consumer stopped"))
        ));
        for control in [
            Control {
                max_rows: Some(1),
                ..Control::default()
            },
            Control {
                max_bytes: Some(1),
                ..Control::default()
            },
        ] {
            assert!(matches!(
                session
                    .query("SELECT * FROM range(100)", [])
                    .for_each_batch(&control, |_| Ok(())),
                Err(Error::LimitExceeded(_))
            ));
        }
        let report = session
            .query("SELECT ?::BIGINT AS id WHERE false", [Value::BigInt(7)])
            .for_each_batch(&control, |_| panic!("empty query emitted data"))
            .unwrap();
        assert_eq!(report.rows, 0);
        assert_eq!(report.schema.field(0).name(), "id");
        session.close().unwrap();
    }

    #[test]
    #[ignore = "requires version-matched Spatial/httpfs extensions"]
    fn callback_panic_stops_listener_before_session_reuse() {
        let directory = tempfile::tempdir().unwrap();
        let mut session = test_session(directory.path());
        let token = Cancellation::default();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            session.query("SELECT 1", []).for_each_batch(
                &Control {
                    cancellation: Some(&token),
                    ..Control::default()
                },
                |_| panic!("fixture panic"),
            )
        }));
        assert!(result.is_err());
        token.cancel();
        assert_eq!(
            session
                .query("SELECT 2", [])
                .for_each_batch(&Control::default(), |_| Ok(()))
                .unwrap()
                .rows,
            1
        );
        session.close().unwrap();
    }

    #[test]
    #[ignore = "requires version-matched Spatial/httpfs extensions"]
    fn spatial_sql_defines_boundary_clip_dissolve_buffer_and_transform_semantics() {
        use crate::duckdb::arrow::array::{Float64Array, Int64Array};
        let directory = tempfile::tempdir().unwrap();
        let mut session = test_session(directory.path());
        session.connection.execute_batch(
            "CREATE TEMP TABLE regions AS SELECT id, wkt::GEOMETRY('EPSG:3857') AS geom FROM (VALUES
             (1, 'POLYGON((0 0,2 0,2 2,0 2,0 0))'),
             (2, 'POLYGON((1 0,3 0,3 2,1 2,1 0))'),
             (3, 'POLYGON((10 10,11 10,11 11,10 11,10 10))')) t(id,wkt);
             CREATE TEMP TABLE points AS SELECT id, wkt::GEOMETRY('EPSG:3857') AS geom FROM (VALUES
             (1, 'POINT(0 1)'), (2, 'POINT(1.5 1)'), (3, 'POINT(20 20)')) t(id,wkt);"
        ).unwrap();
        let mut counts: Vec<i64> = Vec::new();
        session.query("SELECT count(p.id)::BIGINT AS count FROM regions r LEFT JOIN points p ON ST_Intersects(r.geom, p.geom) GROUP BY r.id ORDER BY r.id", [])
            .for_each_batch(&Control::default(), |batch| {
                counts.extend(batch.column(0).as_any().downcast_ref::<Int64Array>().unwrap().values());
                Ok(())
            }).unwrap();
        assert_eq!(counts, vec![2_i64, 1, 0]);
        let mut clipped: Vec<f64> = Vec::new();
        session.query("WITH mask AS (SELECT 'POLYGON((0.5 0.5,2.5 0.5,2.5 1.5,0.5 1.5,0.5 0.5))'::GEOMETRY('EPSG:3857') AS geom) SELECT ST_Area(ST_Intersection(r.geom,m.geom)) AS area FROM regions r JOIN mask m ON ST_Intersects(r.geom,m.geom) ORDER BY r.id", [])
            .for_each_batch(&Control::default(), |batch| {
                clipped.extend(batch.column(0).as_any().downcast_ref::<Float64Array>().unwrap().values());
                Ok(())
            }).unwrap();
        assert_eq!(clipped, vec![1.5_f64, 1.5]);
        session
            .query(
                "SELECT ST_Area(ST_Union_Agg(geom)) AS area FROM regions WHERE id <= ?",
                [Value::Int(2)],
            )
            .for_each_batch(&Control::default(), |batch| {
                assert_eq!(
                    batch
                        .column(0)
                        .as_any()
                        .downcast_ref::<Float64Array>()
                        .unwrap()
                        .value(0),
                    6.0
                );
                Ok(())
            })
            .unwrap();
        let reference = gdal::vector::Geometry::from_wkt("POINT(0 0)")
            .unwrap()
            .buffer(1.0, 8)
            .unwrap()
            .area();
        session
            .query(
                "SELECT ST_Area(ST_Buffer('POINT(0 0)'::GEOMETRY('EPSG:3857'), ?, 8)) AS area",
                [Value::Double(1.0)],
            )
            .for_each_batch(&Control::default(), |batch| {
                assert!(
                    (batch
                        .column(0)
                        .as_any()
                        .downcast_ref::<Float64Array>()
                        .unwrap()
                        .value(0)
                        - reference)
                        .abs()
                        < 1e-10
                );
                Ok(())
            })
            .unwrap();
        session.query("WITH p AS (SELECT ST_Transform('POINT(120 30)'::GEOMETRY('EPSG:4326'), 'EPSG:3857') AS geom) SELECT ST_X(geom), ST_Y(geom) FROM p", [])
            .for_each_batch(&Control::default(), |batch| {
                let x = batch.column(0).as_any().downcast_ref::<Float64Array>().unwrap().value(0);
                let y = batch.column(1).as_any().downcast_ref::<Float64Array>().unwrap().value(0);
                assert!((x - 13358338.895192828).abs() < 1e-6);
                assert!((y - 3503549.843504375).abs() < 1e-6);
                Ok(())
            }).unwrap();
        session.close().unwrap();
    }
}

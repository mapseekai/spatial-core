use super::{PostGisError, Result, WriteStage, sql::quote};
use crate::control::Cancellation;
use deadpool_postgres::{Object, Pool};
use futures::future::BoxFuture;
use std::{future::Future, sync::Arc};
use tokio_postgres::{
    CancelToken, Socket,
    tls::{MakeTlsConnect, TlsConnect},
};

pub(crate) const CLEANUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

pub(crate) struct SridDefinition {
    srid: i32,
    wkt: String,
}

impl SridDefinition {
    pub fn validate(self) -> Result<()> {
        let registered = gdal::spatial_ref::SpatialRef::from_wkt(&self.wkt)
            .map_err(|e| PostGisError::source(0, e))?;
        let expected = gdal::spatial_ref::SpatialRef::from_epsg(self.srid as u32)
            .map_err(|e| PostGisError::source(0, e))?;
        if registered != expected {
            return Err(PostGisError::SchemaMismatch(
                "registered SRID differs from its EPSG definition".into(),
            ));
        }
        Ok(())
    }
}

type Canceller = dyn Fn(CancelToken) -> BoxFuture<'static, std::result::Result<(), tokio_postgres::Error>>
    + Send
    + Sync;

/// Reuses an application pool and a matching TLS connector for query cancellation.
#[derive(Clone)]
pub struct PostGisClient {
    pub(crate) pool: Pool,
    cancel: Arc<Canceller>,
}

impl PostGisClient {
    /// Use the same TLS configuration as the pool. No pool/runtime is created.
    pub fn from_pool<T>(pool: Pool, tls: T) -> Self
    where
        T: MakeTlsConnect<Socket> + Clone + Send + Sync + 'static,
        T::Stream: Send + Sync,
        T::TlsConnect: Send + Sync,
        <T::TlsConnect as TlsConnect<Socket>>::Future: Send,
    {
        Self {
            pool,
            cancel: Arc::new(move |token| {
                let tls = tls.clone();
                Box::pin(async move { token.cancel_query(tls).await })
            }),
        }
    }

    pub(crate) async fn acquire(&self, cancel: &Cancellation) -> Result<ConnectionLease> {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(PostGisError::Cancelled),
            result = self.pool.get() => result.map(ConnectionLease::new).map_err(PostGisError::Pool),
        }
    }

    pub(crate) async fn cancellable<T>(
        &self,
        cancel: &Cancellation,
        token: CancelToken,
        operation: impl Future<Output = Result<T>>,
    ) -> Result<T> {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                match tokio::time::timeout(CLEANUP_TIMEOUT, (self.cancel)(token)).await {
                    Ok(Ok(())) => Err(PostGisError::Cancelled),
                    Ok(Err(error)) => Err(PostGisError::Cancelled.with_cleanup(PostGisError::database(WriteStage::Preparing, error))),
                    Err(_) => Err(PostGisError::Cancelled.with_cleanup(PostGisError::CleanupTimeout)),
                }
            }
            result = operation => result,
        }
    }
}

/// A dropped/aborted operation never returns an unconfirmed connection to the pool.
pub(crate) struct ConnectionLease {
    object: Option<Object>,
    reusable: bool,
}

impl ConnectionLease {
    fn new(object: Object) -> Self {
        Self {
            object: Some(object),
            reusable: false,
        }
    }
    pub fn client(&self) -> &Object {
        self.object.as_ref().expect("lease owns its connection")
    }
    pub fn reusable(&mut self) {
        self.reusable = true;
    }
}

impl Drop for ConnectionLease {
    fn drop(&mut self) {
        if !self.reusable
            && let Some(object) = self.object.take()
        {
            drop(Object::take(object));
        }
    }
}

pub(crate) struct Capabilities {
    pub geometry: String,
    pub raster: Option<String>,
}

impl Capabilities {
    pub async fn read(client: &Object, raster: bool) -> Result<Self> {
        let rows = client.query(
            "SELECT e.extname,n.nspname FROM pg_catalog.pg_extension e JOIN pg_catalog.pg_namespace n ON n.oid=e.extnamespace WHERE e.extname IN ('postgis','postgis_raster')", &[],
        ).await.map_err(|e| PostGisError::database(WriteStage::Preparing, e))?;
        let namespace = |name: &str| {
            rows.iter()
                .find(|r| r.get::<_, &str>(0) == name)
                .map(|r| r.get::<_, String>(1))
        };
        let geometry = namespace("postgis").ok_or_else(|| {
            PostGisError::Unsupported("postgis extension is not installed".into())
        })?;
        let raster_schema = namespace("postgis_raster");
        if raster && raster_schema.is_none() {
            return Err(PostGisError::Unsupported(
                "postgis_raster extension is not installed".into(),
            ));
        }
        Ok(Self {
            geometry,
            raster: raster_schema,
        })
    }

    pub fn geometry_function(&self, function: &str) -> String {
        format!("{}.{}", quote(&self.geometry), function)
    }

    pub async fn configure_transaction(&self, client: &Object) -> Result<()> {
        // PostGIS functions also resolve unqualified types in their own SQL bodies.
        let mut schemas = vec!["pg_catalog".to_owned(), quote(&self.geometry)];
        if let Some(raster) = &self.raster
            && raster != &self.geometry
        {
            schemas.push(quote(raster));
        }
        schemas.push("pg_temp".into());
        client
            .query_one(
                "SELECT pg_catalog.set_config('search_path',$1,true)",
                &[&schemas.join(",")],
            )
            .await
            .map_err(|e| PostGisError::database(WriteStage::Preparing, e))?;
        Ok(())
    }
    pub fn raster_function(&self, function: &str) -> Result<String> {
        self.raster
            .as_ref()
            .map(|schema| format!("{}.{}", quote(schema), function))
            .ok_or_else(|| {
                PostGisError::Unsupported("postgis_raster extension is not installed".into())
            })
    }

    pub async fn srid_definition(&self, client: &Object, srid: i32) -> Result<SridDefinition> {
        let row = client
            .query_opt(
                &format!(
                    "SELECT auth_name,auth_srid,srtext FROM {}.spatial_ref_sys WHERE srid=$1",
                    quote(&self.geometry)
                ),
                &[&srid],
            )
            .await
            .map_err(|e| PostGisError::database(WriteStage::Preparing, e))?;
        let Some(row) = row else {
            return Err(PostGisError::InvalidInput(format!(
                "SRID {srid} is not registered"
            )));
        };
        if row.get::<_, Option<&str>>(0) != Some("EPSG")
            || row.get::<_, Option<i32>>(1) != Some(srid)
        {
            return Err(PostGisError::Unsupported(
                "target SRID must map to the same EPSG identifier".into(),
            ));
        }
        let wkt: String = row.get::<_, Option<String>>(2).ok_or_else(|| {
            PostGisError::SchemaMismatch("registered SRID has no WKT definition".into())
        })?;
        Ok(SridDefinition { srid, wkt })
    }
}

#[cfg(test)]
mod tests {
    use super::SridDefinition;
    use crate::postgis::{
        ColumnName, WriteControl,
        raster::{PostGisRasterWriter, RasterSource},
        test_support::{TestDb, points, raster_file},
        vector::{VectorSource, read_metadata},
    };

    #[tokio::test]
    async fn worker_srid_validation_preserves_definition_checks_and_errors() -> anyhow::Result<()> {
        use crate::postgis::{PostGisError, WriteLimits, stream::SourceStream};
        for registered in [4326, 3857] {
            let definition = SridDefinition {
                srid: 4326,
                wkt: gdal::spatial_ref::SpatialRef::from_epsg(registered)?.to_wkt()?,
            };
            let mut stream = SourceStream::start(WriteLimits::default(), |metadata, sender| {
                metadata.send(()).map_err(|_| PostGisError::Cancelled)?;
                sender.validate_srid()?;
                sender.send(vec![1])
            });
            stream.metadata().await?;
            let result = stream.validate_srid(definition).await;
            if registered == 4326 {
                result?;
                assert!(stream.rows.recv().await.is_some());
                assert_eq!(stream.finish().await?, 1);
            } else {
                assert!(matches!(result, Err(PostGisError::SchemaMismatch(_))));
                stream.stop().await?;
            }
        }
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires dedicated PostGIS Raster database"]
    async fn extension_resolution_is_transaction_local() -> anyhow::Result<()> {
        let db = TestDb::new().await?;
        let result = async {
            db.pool
                .get()
                .await?
                .batch_execute("SET search_path=pg_catalog")
                .await?;
            db.vector("points")?
                .write(VectorSource::new(points()), &WriteControl::default())
                .await?;
            let metadata = read_metadata(
                &db.client,
                &db.table("points")?,
                &ColumnName::new("geom")?,
                &WriteControl::default(),
            )
            .await?;
            assert_eq!(metadata.feature_count, 3);
            let file = raster_file()?;
            let table = db.table("pixels")?;
            PostGisRasterWriter::new(&db.client, table.clone())
                .write(RasterSource::new(file.path()), &WriteControl::default())
                .await?;
            let metadata = crate::postgis::raster::read_metadata(
                &db.client,
                &table,
                &ColumnName::new("rast")?,
                &WriteControl::default(),
            )
            .await?;
            assert_eq!(metadata.tile_count, 1);
            assert_eq!(
                db.pool
                    .get()
                    .await?
                    .query_one("SHOW search_path", &[])
                    .await?
                    .get::<_, &str>(0),
                "pg_catalog"
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        db.cleanup().await?;
        result
    }
}

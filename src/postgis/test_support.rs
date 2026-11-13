use super::{
    PostGisClient, Result, TableName,
    vector::{PostGisVectorWriter, VectorWriteOptions},
};
use deadpool_postgres::{Manager, Pool};
use std::path::PathBuf;

pub(crate) struct TestDb {
    pub pool: Pool,
    pub client: PostGisClient,
    pub schema: String,
}

impl TestDb {
    pub async fn new() -> anyhow::Result<Self> {
        let dsn = std::env::var("SPATIAL_CORE_TEST_POSTGRES_DSN")?;
        let pool = Pool::builder(Manager::new(dsn.parse()?, tokio_postgres::NoTls))
            .max_size(1)
            .build()?;
        let schema = format!("sc_{}", uuid::Uuid::new_v4().simple());
        pool.get()
            .await?
            .batch_execute(&format!("CREATE SCHEMA {schema}"))
            .await?;
        Ok(Self {
            client: PostGisClient::from_pool(pool.clone(), tokio_postgres::NoTls),
            pool,
            schema,
        })
    }
    pub fn table(&self, name: &str) -> Result<TableName> {
        TableName::new(&self.schema, name)
    }
    pub fn vector(&self, name: &str) -> Result<PostGisVectorWriter<'_>> {
        Ok(PostGisVectorWriter::new(&self.client, self.table(name)?))
    }
    pub async fn cleanup(self) -> anyhow::Result<()> {
        self.pool
            .get()
            .await?
            .batch_execute(&format!("DROP SCHEMA {} CASCADE", self.schema))
            .await?;
        Ok(())
    }
    pub fn replace() -> VectorWriteOptions {
        VectorWriteOptions::default().mode(super::WriteMode::ReplaceTable)
    }
}

pub(crate) fn json_file(text: &str) -> anyhow::Result<tempfile::NamedTempFile> {
    let file = tempfile::NamedTempFile::with_suffix(".geojson")?;
    std::fs::write(file.path(), text)?;
    Ok(file)
}

pub(crate) fn points() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/points.geojson")
}

pub(crate) fn failing_vector(valid_rows: usize) -> anyhow::Result<(tempfile::TempDir, PathBuf)> {
    use crate::gdal_ext::vector::vector_translate::vector_translate;
    let directory = tempfile::tempdir()?;
    let source = gdal::Dataset::open(points())?;
    let geopackage = directory.path().join("points.gpkg");
    drop(vector_translate(
        &source,
        geopackage.as_path().try_into()?,
        Some(vec!["-f", "GPKG", "-nln", "points"].try_into()?),
    )?);
    let path = directory.path().join("broken.vrt");
    std::fs::write(
        &path,
        format!(
            r#"<OGRVRTDataSource><OGRVRTLayer name="broken">
<SrcDataSource relativeToVRT="1">points.gpkg</SrcDataSource>
<SrcSQL dialect="SQLITE">SELECT geom, json_extract(CASE WHEN fid &gt; {valid_rows} THEN 'invalid-json' ELSE '{{"a":1}}' END, '$.a') AS value FROM points</SrcSQL>
<GeometryType>wkbPoint</GeometryType><LayerSRS>EPSG:4326</LayerSRS>
<Field name="value" type="Integer"/>
</OGRVRTLayer></OGRVRTDataSource>"#
        ),
    )?;
    Ok((directory, path))
}

pub(crate) fn raster_file() -> anyhow::Result<tempfile::NamedTempFile> {
    let file = tempfile::NamedTempFile::with_suffix(".tif")?;
    let driver = gdal::DriverManager::get_driver_by_name("GTiff")?;
    let mut dataset = driver.create_with_band_type::<u16, _>(file.path(), 5, 3, 2)?;
    dataset.set_spatial_ref(&gdal::spatial_ref::SpatialRef::from_epsg(4326)?)?;
    dataset.set_geo_transform(&[100., 0.1, 0.02, 30., 0.01, -0.1])?;
    for band in 1..=2 {
        let mut buffer =
            gdal::raster::Buffer::new((5, 3), (0..15).map(|v| v + band as u16 * 100).collect());
        let mut output = dataset.rasterband(band)?;
        output.write((0, 0), (5, 3), &mut buffer)?;
        output.set_no_data_value(Some(65535.))?;
    }
    dataset.flush_cache()?;
    drop(dataset);
    make_server_readable(file.path())?;
    Ok(file)
}

pub(crate) fn make_server_readable(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Synthetic OutDB fixtures are also read by the CI container's postgres UID.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644))?;
    }
    Ok(())
}

/// Local protocol proxy for deterministic DDL/COMMIT gates and failed CancelRequest connections.
pub(crate) struct TestProxy {
    pub pool: Pool,
    pub reached: tokio::sync::oneshot::Receiver<()>,
    resume: Option<tokio::sync::oneshot::Sender<()>>,
    worker: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl TestProxy {
    pub async fn new(
        statement: Option<String>,
        refuse_new_connections: bool,
    ) -> anyhow::Result<Self> {
        use tokio::{net::TcpListener, sync::oneshot, task::JoinSet};
        use tokio_postgres::config::{Host, SslMode};
        let original: tokio_postgres::Config =
            std::env::var("SPATIAL_CORE_TEST_POSTGRES_DSN")?.parse()?;
        let Some(Host::Tcp(host)) = original.get_hosts().first() else {
            anyhow::bail!("protocol fault tests require a TCP test DSN");
        };
        let address = (
            host.clone(),
            original.get_ports().first().copied().unwrap_or(5432),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let mut config = tokio_postgres::Config::new();
        config
            .host("localhost")
            .port(listener.local_addr()?.port())
            .ssl_mode(SslMode::Disable);
        if let Some(user) = original.get_user() {
            config.user(user);
        }
        if let Some(password) = original.get_password() {
            config.password(password);
        }
        if let Some(database) = original.get_dbname() {
            config.dbname(database);
        }
        let pool = Pool::builder(Manager::new(config, tokio_postgres::NoTls))
            .max_size(1)
            .build()?;
        let (reached, at_statement) = oneshot::channel();
        let (resume, released) = oneshot::channel();
        let mut gate = statement.map(|sql| (sql, reached, released));
        let worker = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                let (client, _) = listener.accept().await?;
                let address = address.clone();
                let gate = gate.take();
                connections.spawn(async move { proxy_connection(client, address, gate).await });
                if refuse_new_connections {
                    break;
                }
                while connections.try_join_next().is_some() {}
            }
            drop(listener);
            while connections.join_next().await.is_some() {}
            Ok(())
        });
        Ok(Self {
            pool,
            reached: at_statement,
            resume: Some(resume),
            worker,
        })
    }

    pub fn resume(&mut self) {
        if let Some(resume) = self.resume.take() {
            let _ = resume.send(());
        }
    }
}

impl Drop for TestProxy {
    fn drop(&mut self) {
        self.worker.abort();
    }
}

type ProtocolGate = (
    String,
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
);

async fn proxy_connection(
    client: tokio::net::TcpStream,
    address: (String, u16),
    mut gate: Option<ProtocolGate>,
) -> std::io::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let server = tokio::net::TcpStream::connect(address).await?;
    client.set_nodelay(true)?;
    server.set_nodelay(true)?;
    let (mut input, mut output) = client.into_split();
    let (mut replies, mut requests) = server.into_split();
    let forward = async {
        let length = input.read_u32().await?;
        let mut startup = vec![
            0;
            length
                .checked_sub(4)
                .filter(|n| *n < 65536)
                .ok_or(std::io::ErrorKind::InvalidData)? as usize
        ];
        input.read_exact(&mut startup).await?;
        requests.write_u32(length).await?;
        requests.write_all(&startup).await?;
        loop {
            let kind = input.read_u8().await?;
            let length = input.read_u32().await?;
            let mut body = vec![
                0;
                length
                    .checked_sub(4)
                    .filter(|n| *n < 16 * 1024 * 1024)
                    .ok_or(std::io::ErrorKind::InvalidData)?
                    as usize
            ];
            input.read_exact(&mut body).await?;
            if matches!(kind, b'P' | b'Q')
                && gate
                    .as_ref()
                    .is_some_and(|(sql, _, _)| body.windows(sql.len()).any(|w| w == sql.as_bytes()))
            {
                let (_, reached, released) = gate.take().expect("matching gate exists");
                let _ = reached.send(());
                let _ = released.await;
            }
            requests.write_u8(kind).await?;
            requests.write_u32(length).await?;
            requests.write_all(&body).await?;
        }
    };
    tokio::select! {
        result = forward => result,
        result = tokio::io::copy(&mut replies, &mut output) => result.map(|_| ()),
    }
}

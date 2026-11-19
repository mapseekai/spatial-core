//! Deployment-time provisioning. Run once, then keep the deployment directory immutable.
use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    let directory =
        PathBuf::from(std::env::var("SPATIAL_CORE_DUCKDB_EXTENSION_DIR")?).canonicalize()?;
    let public_key =
        std::fs::read_to_string(std::env::var("SPATIAL_CORE_DUCKDB_SPATIAL_PUBLIC_KEY")?)?;
    let scratch = tempfile::tempdir()?;
    let repositories = scratch.path().join("repositories");
    let secrets = scratch.path().join("secrets");
    let settings = duckdb::Config::default()
        .enable_autoload_extension(false)?
        .with(
            "extension_directory",
            directory
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("UTF-8 deployment path required"))?,
        )?
        .with(
            "extension_repository_directory",
            repositories
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("UTF-8 scratch path required"))?,
        )?
        .with(
            "secret_directory",
            secrets
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("UTF-8 scratch path required"))?,
        )?
        .with("allow_persistent_secrets", "false")?
        .with("allow_extension_repositories", "allowed")?;
    let connection = duckdb::Connection::open_in_memory_with_flags(settings)?;
    // Paths and public keys are explicit operator inputs, never SQL fragments.
    anyhow::ensure!(!public_key.contains('\0'), "invalid public key");
    let prefix = directory.to_str().unwrap().replace('\'', "''");
    let key = public_key.replace('\'', "''");
    connection.execute_batch(&format!(
        "CREATE EXTENSION REPOSITORY spatial_core WITH PREFIX '{prefix}' USING PUBLIC KEY '{key}'; INSTALL spatial FROM spatial_core;"
    ))?;
    let platform: String = connection.query_row("PRAGMA platform", [], |row| row.get(0))?;
    let source = directory
        .join("v2.0.0")
        .join(platform)
        .join("spatial.duckdb_extension");
    let installed = source
        .parent()
        .unwrap()
        .join("repositories/spatial_core/spatial.duckdb_extension");
    anyhow::ensure!(
        std::fs::read(source)? == std::fs::read(installed)?,
        "installed Spatial differs from source; provision a new deployment directory"
    );
    connection.execute_batch("LOAD spatial FROM spatial_core")?;
    let contract: i32 =
        connection.query_row("SELECT spatial_core_gdal_read_contract()", [], |row| {
            row.get(0)
        })?;
    anyhow::ensure!(contract == 1, "unexpected GDAL read contract");
    let allocator_contract: i32 =
        connection.query_row("SELECT spatial_core_allocator_contract()", [], |row| {
            row.get(0)
        })?;
    anyhow::ensure!(allocator_contract == 1, "unexpected allocator contract");
    drop(connection);
    scratch.close()?;
    println!("Provisioned and signature-verified patched Spatial");
    Ok(())
}

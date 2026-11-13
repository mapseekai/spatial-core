# spatial-core

独立 Rust 底层 GIS 能力库，从 MapSeek 的 `services/spatial-forge/spatial-core` 提取。不需要 MapSeek workspace、服务进程、业务数据库或相邻代码仓库即可编译及运行默认测试。

## 能力与边界

- `raster::convert`：GDAL translate、warp、DEM、polygonize、rasterize、fill-nodata、sieve 和 contour，以及进度回调与取消。
- `raster::tiling`：独立的惰性窗口计算，支持整幅、分块及 overlap。
- `vector::convert`：GeoParquet、PGDump 文件转换。
- `postgis::vector`：精确字段 schema、GDAL 输入、事务导入及数据库元数据。
- `postgis::raster`：InDB/OutDB、整幅/分块、重投影、栅格约束及数据库元数据。
- `control`：协作式取消与输入错误分类。
- `config::gdal`：显式 GDAL 选项及 worker 线程配置作用域。
算子注册/执行协议留在未来 server；完整 DAG、调度和业务编排留在 gis-workflow。底层转换函数不是业务算子目录。本库不包含租户/项目授权、业务资源 ID、Temporal worker 或 HTTP 服务。

## 本地使用

源码仓库：[mapseekai/spatial-core](https://github.com/mapseekai/spatial-core)。尚未发布到 crates.io；使用与分发权限见 [LICENSE](LICENSE)。获授权的调用方克隆仓库后可以使用本地 path：

```toml
[dependencies]
spatial-core = { path = "../spatial-core" }
```

公开 crate 名称仍为 `spatial_core`，版本仍为 0.1.0。示例可直接运行，不需要外部数据：

```sh
cargo run --locked --example raster_convert
```

示例在临时目录生成微型 GeoTIFF，调用公开转换 API，核对输出像元和空间参考，并在结束时清理。同步 GDAL 处理应放在调用者的 blocking 线程；异步导入函数自行管理 blocking reader，但 PostgreSQL 连接池和运行时仍由调用方提供。

## PostGIS 导入

连接池及 Tokio runtime 由应用提供。池大小为 1 也可使用；TLS connector 必须与创建池时一致。

```rust
use spatial_core::postgis::{PostGisClient, TableName, WriteControl};
use spatial_core::postgis::vector::{PostGisVectorWriter, VectorSource};

async fn example(pool: deadpool_postgres::Pool) -> anyhow::Result<()> {
    let db = PostGisClient::from_pool(pool, tokio_postgres::NoTls);
    let report = PostGisVectorWriter::new(&db, TableName::new("gis", "roads")?)
        .target_srid(3857)
        .write(VectorSource::new("roads.gpkg").layer_name("roads"), &WriteControl::default())
        .await?;
    println!("committed {} features", report.feature_count);
    Ok(())
}
```

默认创建新表、保留源字段名和 CRS；已有表不会被覆盖。显式选择 `ReplaceTable` 会重建表及 identity，`ReplaceRows` 校验结构后清空行、保留原表与约束并延续 identity。两种替换均在一次事务内完成，拒绝分区、继承、foreign table、视图及 RLS 目标。延迟 trigger 在最终校验前执行，最终行数不符则回滚；允许既有 trigger 在表约束内改写值，报告范围反映实际结果。替表使用 `RESTRICT`，不会级联删除依赖。

返回报告表示 COMMIT 已确认。收到 `CommitOutcomeUnknown` 时必须核对数据库状态再决定重试。取消应调用 token 后继续 await；取消投递失败会丢弃连接，投递与回滚各最多等待 5 秒。`Cleanup` 表示清理未确认，不能假定服务端已即时停止；原生 GDAL I/O 不受这个数据库清理期限约束。直接丢弃 future 无法保证原生 GDAL I/O 已结束。替换会持有排他锁，旧快照还受 PostgreSQL 的 TRUNCATE/新表可见性限制。

栅格使用 `PostGisRasterWriter`，默认 InDB、256×256、无 overlap、源 CRS。`RasterStorage::OutDb { server_path }` 单独指定数据库服务器可读的持久路径。重投影默认 nearest，OutDB 重投影须先持久化输出。完整接口、目录和迁移表见 [重构设计](docs/POSTGIS_REFACTOR.md)。

## 显式 GDAL 配置

入库不再读取 CWD 下的 TOML。通过 `VectorSource::gdal_options` / `RasterSource::gdal_options` 传入 `GdalOptions::default().set("KEY", "VALUE")?`；选项在实际打开数据的 blocking worker 上应用，退出或 panic 时恢复原线程值。Debug 只输出配置键。没有已有设置时，worker 的 HTTP 连接及请求超时均默认 30 秒；这不是所有驱动 I/O 的硬截止时间。

进程初始化仍可调用 `apply_gdal_options`，或者 `load_config_file(path, override_env)` 读取指定 GDAL 原生 INI。远程存储凭据由应用显式提供；本库不修改数据库扩展配置或服务器存储权限。

## 构建与测试

固定 Rust 1.98.1 / edition 2024，独立 Cargo.lock，依赖均为 crates.io 包，没有 MapSeek 内部 path/git 依赖。默认即包含 GDAL 和 PostGIS 适配，没有 feature 矩阵。

原生依赖：GDAL 开发库及匹配的运行库、libclang（`gdal` 启用 bindgen）、C/C++ 编译工具及 pkg-config；GDAL 的 PROJ 数据必须与安装匹配。GDAL Rust 绑定 0.19 支持 GDAL 3.4+；本地验证版本和额外驱动要求见 [测试说明](docs/TESTING.md)。GeoParquet 导出另需 GDAL Parquet driver。

```sh
# macOS，在需要时安装开发依赖
brew install gdal llvm pkg-config
# Ubuntu 24.04，在需要时安装开发依赖
sudo apt-get install libgdal-dev gdal-bin libclang-dev pkg-config

bash scripts/ci/check.sh
```

`check.sh` 执行结构/测试归属检查、格式、Clippy、默认测试、Rustdoc、可运行示例和 diff 空白检查。数据库测试默认忽略，必须对专用 PostGIS + postgis_raster 测试库单独执行：

```sh
SPATIAL_CORE_TEST_POSTGRES_DSN='host=127.0.0.1 port=5432 user=postgres dbname=spatial_core_test' \
  bash scripts/ci/test-postgis.sh
```

未提供 DSN 会直接报错，不会连接默认或个人业务数据库，也不会将跳过当成成功。详见 [测试说明](docs/TESTING.md)。

## 数据契约与迁移

本次是破坏性重构，移除了 `pg::PgCopy`、两类旧 `copy_to_postgis`、隐式 TOML/S3 配置入口、独立空表/字段恢复入口和重复 GDAL 直写路径。旧调用方需迁移到两类 writer；旧 `anyhow` downcast 改为匹配 `PostGisError`。公开 crate 名称及许可保持原状。

- identity 固定为 bigint；字段有序，保留大小写，名称冲突报错。整数数组区分 NULL 与空数组。
- 最终类型支持 numeric、boolean、date、timestamp、json/jsonb 等精确契约；decimal 文本不经过 f64。原生 OGR DateTime 仍拒绝，避免丢失小数秒或猜测时区；可用支持的 driver open option（如 GeoJSON `DATE_AS_STRING=YES`）保留原始时间文本后显式指定目标类型。
- 文本映射 real/double precision 必须精确：`0.5` 可以，`0.1` 或 `9007199254740993` 应用 NUMERIC；原生 OGR Real 保持已有二进制浮点值。
- ZIP 的归档根和明确成员 URI 均可使用；metadata 查询在目标读锁内读取一致快照，混合 SRID 的 raster extent 为 `None`。
- geometry 支持 Point/LineString/Polygon 及对应 Multi，维度 XY/XYZ/XYM/XYZM；不自动 makevalid、降维或忽略失败要素。空源须显式允许且能确定完整 schema，EMPTY 几何的 extent 为 `None`。
- 目标 SRID 需对应数据库中真实一致的 EPSG 定义，不再限定 4326/3857；有效但无 authority 的源 WKT 可显式重投影。
- raster 支持 UInt8/UInt16/Int16/UInt32/Int32/Float32/Float64、多波段、NoData、skew 与边缘裁块。独立 mask、非默认 scale/offset、无仿射的 GCP/RPC 输入须先显式转换。

[完整迁移与文件组织](docs/POSTGIS_REFACTOR.md) · [本次验证记录](docs/POSTGIS_VALIDATION.md) · [历史提取记录](docs/EXTRACTION.md)。相邻 MapSeek 内嵌副本和消费者依赖未修改，需按新接口单独迁移。

## 许可

保留原仓库 [All rights reserved 声明](LICENSE)，未新增公开许可。`publish = false` 防止意外发包。GitHub 仓库公开可见不等于授予开源许可；本项目没有改为 MIT，也未发布 crate。公开快照与审查基线见 [公开说明](docs/PUBLICATION.md)。[第三方说明](docs/third-party-notices.md) 不替代各依赖的原始许可。

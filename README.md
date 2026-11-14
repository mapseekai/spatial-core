# spatial-core

独立 Rust 底层 GIS 能力库，从 MapSeek 的 `services/spatial-forge/spatial-core` 提取。不需要 MapSeek workspace、服务进程、业务数据库或相邻代码仓库即可编译及运行默认测试。

## 能力与边界

- `raster::{Translate, Warp, BuildVrt, Dem, Rasterize, Polygonize, Contour, FillNodata, Sieve}`：拥有输入描述的栅格转换与处理 Builder，统一进度、取消、诊断和显式关闭；utility 类操作另有 `Native*` 参数入口。
- `raster::convert`：保留现有接受 GDAL 句柄的低层接口，供 PostGIS 等内部调用。
- `raster::tiling`：独立的惰性窗口计算，支持整幅、分块及 overlap。
- `vector::Translate` / `NativeTranslate`：类型化或原生参数的矢量转换，支持显式创建、替层及追加，以及裁剪、简化、顶点加密、集合拆分、类型转换、修复和 SQL 处理。
- `dataset::Source` / `operation`：输入描述、借用式进度控制、取消、诊断与类型化错误。
- `postgis::vector`：精确字段 schema、GDAL 输入、事务导入及数据库元数据。
- `postgis::raster`：InDB/OutDB、整幅/分块、重投影、栅格约束及数据库元数据。
- `control`：协作式取消与输入错误分类。
- `config::gdal`：显式 GDAL 选项及 worker 线程配置作用域。
算子注册/执行协议留在未来 server；完整 DAG、调度和业务编排留在 gis-workflow。底层转换函数不是业务算子目录。本库不包含租户/项目授权、业务资源 ID、Temporal worker 或 HTTP 服务。

[GDAL 能力封装设计](docs/GDAL_API_DESIGN.md) 说明矢量转换基线；[栅格与矢量处理设计](docs/GDAL_PROCESSING_DESIGN.md) 说明本轮扩展的 API、文件组织和算法边界。旧 `to_geoparquet` / `to_pg_dump` 已移除，其隐式重投影、降维、修复及跳过失败行为改为调用方显式选择。

## 本地使用

源码仓库：[mapseekai/spatial-core](https://github.com/mapseekai/spatial-core)。尚未发布到 crates.io；使用与分发权限见 [LICENSE](LICENSE)。获授权的调用方克隆仓库后可以使用本地 path：

```toml
[dependencies]
spatial-core = { path = "../spatial-core" }
```

公开 crate 名称仍为 `spatial_core`，版本仍为 0.1.0。示例可直接运行，不需要外部数据：

```sh
cargo run --locked --example raster_convert
cargo run --locked --example vector_convert
cargo run --locked --example raster_process
cargo run --locked --example vector_process
```

示例在临时目录生成微型 GeoTIFF，调用公开转换 API，核对输出像元和空间参考，并在结束时清理。同步 GDAL 处理应放在调用者的 blocking 线程；异步导入函数自行管理 blocking reader，但 PostgreSQL 连接池和运行时仍由调用方提供。

## 矢量格式转换

```rust
use spatial_core::dataset::Source;
use spatial_core::vector::{Dimensions, Translate};

fn convert() -> spatial_core::operation::Result<()> {
    let source = Source::new("roads.shp").open_option("ENCODING", "UTF-8");
    let report = Translate::new(source, "roads.gpkg")
        .driver("GPKG")
        .where_clause("class = 'primary'")
        .reproject_to("EPSG:3857")
        .dimensions(Dimensions::Xy)
        .layer_option("SPATIAL_INDEX", "YES")
        .run()?;
    println!("{} diagnostics", report.diagnostics().len());
    Ok(())
}
```

默认创建不存在的本地输出，不覆盖已有数据集或伴随文件；默认不修复、降维或跳过失败。已有数据集用 `CreateLayer` / `ReplaceLayer` / `AppendFeatures` 和显式 `layer_name`，追加会先检查 schema、几何与 CRS。`dataset_option`、`layer_option`、`output_open_option` 分别对应不同阶段；高级 GDAL 参数、VSI 输出用独立的 `NativeTranslate`，原生入口沿用 GDAL 的写入语义。

`run_with(&mut Control)` 借用 `Cancellation` 和 `FnMut(&Progress) -> ControlFlow<()> + Send`，无需 `'static`。共享回调桥接串行调用闭包并捕获 panic；raster 现有回调也要求 `Send`。输入、输出在调用线程打开和关闭，异步应用自行使用 `spawn_blocking`。成功返回表示关闭已完成；取消/失败可能留下部分输出，不能据此假定回滚或直接重试追加。GDAL 没有回调的路径无法保证中途响应取消。Shapefile 的类型化 `CreateLayer` 要求目录目标；更新图层名限字母数字、`_`、`-`；多几何列追加要求同名同序。

`Report` / `Error` 保留最多 64 条、每条 4 KiB 的原生诊断；错误另含阶段、输出状态和分别保留的关闭错误。驱动可能截短字段名、调整类型或将 NULL 字符串读为空串；成功不等于无损转换。可运行示例支持 `cargo run --example vector_convert -- INPUT OUTPUT DRIVER [LAYER]`，转换后读回核对要素数。

源属性存在名为 `fid` 的列时，GPKG 的默认 FID 规则可能把它用作主键或因与内部 FID 不一致而拒绝；可显式 `.layer_option("FID", "row_id")` 保留该普通字段。Shapefile 中文字符串输出应按需要显式 `.layer_option("ENCODING", "UTF-8")`。实测数据与限制见 [GDAL 验证记录](docs/GDAL_VALIDATION.md)。

## 栅格转换与处理

```rust
use spatial_core::raster::{Translate, Warp, FillNodata, Resampling};

fn process() -> spatial_core::operation::Result<()> {
    let _ = Translate::new("image.tif", "rgb.cog.tif")
        .driver("COG").bands([1, 2, 3])
        .creation_option("COMPRESS", "DEFLATE").run()?;
    let _ = Warp::new(["west.tif", "east.tif"], "mosaic.tif")
        .target_srs("EPSG:3857").resolution(10.0, 10.0)
        .resampling(Resampling::Bilinear).run()?;
    let _ = FillNodata::new("dem.tif", "filled.tif")
        .max_distance(100.0).smoothing_iterations(1).run()?;
    Ok(())
}
```

所有新 Builder 接受同一个 `Source`，提供 `run` / `run_with`，返回已检查关闭结果的 `Report`。类型化输出必须是不存在的本地数据集；`NativeTranslate` / `NativeWarp` / `NativeDem` / `NativeRasterize` / `NativeBuildVrt` 接受 GDAL utility argv，并允许原生覆盖语义。算法失败可能留下部分输出，检查 `Error::output_state()`，不要据此假定回滚。目标目录需由调用方独占，预检不是跨进程原子锁。

`FillNodata` / `Sieve` 先复制到新 GTiff，再处理选定波段；所有源波段须具有相同像元类型，其他波段和独立 mask 保留复制结果。Sieve 接受有符号/无符号 8/16/32 位整数。FillNodata 实际插值仅接受 Float32、8/16 位整数，避免原生缓冲静默降低高精度数据的精度；全有效输入直接复制。显式 `Mask::Band` 要求 UInt8。

Polygonize 默认使用有效性 mask 和 Int64 分类通道，浮点分类须显式 `floating_values(true)`，受 GDAL Float32 精度约束。Contour 不支持独立/alpha 或多波段联合 NoData mask，需先归一化为单波段 NoData 或显式 `ignore_mask(true)`。Rasterize 的 `assign_srs` 只改标签，不重投影。COG 与 FillNodata 的中间文件写入独占临时目录，遵循调用者设置的 `CPL_TMPDIR` 所在磁盘。

`BuildVrt` 创建虚拟数据集，不复制像元；关闭并不解除对源文件、驱动及访问配置的依赖。`BuildVrt` / `NativeBuildVrt` 会拒绝含 `Source::open_option` 的输入；需要这些选项时，先用 `Translate` 输出 VRT 代理，再将代理用于拼接。多输入 Warp/VRT 持有所有源句柄到输出关闭；GDAL 分块执行不代表所有算法都有固定内存上限，分类复杂度高的 polygonize/sieve 仍可能耗费大量内存。

## 矢量处理

```rust
use spatial_core::vector::{Clip, Translate};

fn process() -> spatial_core::operation::Result<()> {
    let _ = Translate::new("roads.gpkg", "clipped.gpkg")
        .clip_source(Clip::Bounds([100.0, 20.0, 120.0, 40.0]))
        .simplify(0.001).reproject_to("EPSG:3857").run()?;
    Ok(())
}
```

源裁剪、简化及 `segmentize` 的距离使用源坐标单位；`clip_destination` 在重投影之后执行。Builder 方法顺序不改变 GDAL 的算法顺序；简化与顶点加密需要分开执行。`explode_collections` 可能增加要素数并重新分配 FID；几何处理与类型化追加不组合，先写中间结果或使用 Native。Buffer、dissolve、属性计算复用 `sql(query, SqlDialect::Sqlite)`，需要相应 GDAL SQLite/GEOS 能力。

具体 `geometry_type`（如 `MultiPolygon`）必须同时指定 `dimensions`，避免 Z/M 被原生类型转换静默丢弃；三种语义转换 `PromoteToMulti` / `ConvertToLinear` / `ConvertToCurve` 默认保留维度。完整参数边界、文件组织与兼容性说明见 [处理设计](docs/GDAL_PROCESSING_DESIGN.md)，实测范围见 [处理验证记录](docs/GDAL_PROCESSING_VALIDATION.md)。

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

原生依赖：GDAL 开发库及匹配的运行库、libclang（`gdal` 启用 bindgen）、C/C++ 编译工具及 pkg-config；GDAL 的 PROJ 数据必须与安装匹配。新栅格/矢量 Builder 要求构建与运行的 GDAL 都至少为 3.7，以检查显式关闭结果；带空格/逗号/引号的字段投影需 3.9+。不能据此假定兼容后续所有版本；macOS CI 固定 GDAL 3.12.2，已知 3.13.3 的 C API 变动会导致编译失败。隔离环境配置和额外驱动要求见 [测试说明](docs/TESTING.md)。GeoParquet 导出另需 GDAL Parquet driver。

```sh
# macOS，先按 docs/TESTING.md 配置 GDAL 3.12.2 的隔离环境
brew install llvm pkg-config
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

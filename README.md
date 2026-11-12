# spatial-core

独立 Rust 底层 GIS 能力库，从 MapSeek 的 `services/spatial-forge/spatial-core` 提取。不需要 MapSeek workspace、服务进程、业务数据库或相邻代码仓库即可编译及运行默认测试。

## 能力与边界

- `raster::convert`：GDAL translate、warp、DEM、polygonize、rasterize、fill-nodata、sieve 和 contour，以及进度回调与取消。
- `raster::copy`：按块/整图、InDB/OutDB 栅格 PostGIS 写入、NoData 与 CRS 校验。
- `vector`：OGR 矢量转换、GeoParquet、PGDump、PostGIS 导入及元数据。
- `pg`：类型映射、事务性替表、COPY、几何索引、空矢量契约和字段恢复。
- `control`：协作式取消与输入错误分类，不管理业务任务生命周期。
- `config::gdal`：显式 GDAL 选项、原生 INI 文件及保留的兼容配置入口。

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

## 配置兼容性

新调用者可以在应用启动阶段显式调用 `config::gdal::apply_gdal_options(&[("GDAL_HTTP_TIMEOUT", "60")])`，或者使用 `load_config_file(path, override_env)` 读取指定的 GDAL 原生 INI。

现有 `apply_gdal_config(s3).await` 的行为没有变化：首次成功调用从当前工作目录的 `config/gdal_config.toml` 读取 `[gdal]` 中的字符串，键转大写，缓存到进程结束；缺少文件时缓存空配置。后续调用重新应用该缓存，再设置当前线程的 S3 配置。`vector::copy_to_postgis` 仍会调用它。它是已保留的迁移兼容点，不能声称本库所有调用都不读应用配置；是否去掉隐式读取须在后续迁移中显式决定。

S3 选项是 OS 线程局部的，async caller 设置不会自动传到 `spawn_blocking`。在真正打开数据的线程调用 `apply_thread_local_gdal_s3_config`。传入 `None` 保持已有行为，不会清除该线程已有凭据；多凭据应用须显式隔离线程/配置生命周期。真实 S3 凭据和 TLS 策略由调用方负责。

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

- 新矢量 `_id`、`create_gid` 和栅格 tile ID 使用 bigint identity，不能收窄到 int32。
- 矢量 COPY、索引和 ANALYZE 在同一提交前完成；失败回滚替换，重试替表而非追加。栅格失败/取消保留原已提交表。
- 支持 integer、integer[]、integer64、real、string、date；integer[] 保留空数组与 NULL 的区别。DateTime、其他列表及二进制仍显式拒绝，不偷偷转换类型。
- 当前矢量 COPY 的目标 SRID 保留原限制：4326 或 3857。GDAL warp 等底层函数仍可接受 GDAL 支持的其他 CRS。
- 部分便捷转换函数保留既有 `_id`/`geom`、4326、2D、makevalid 等默认约定；这不是新的通用数据策略。详见 [提取与迁移记录](docs/EXTRACTION.md)。

原 MapSeek 内嵌副本和消费者依赖未修改。本项目独立可开发，不代表 MapSeek 已迁移到它。

## 许可

保留原仓库 [All rights reserved 声明](LICENSE)，未新增公开许可。`publish = false` 防止意外发包。GitHub 仓库公开可见不等于授予开源许可；本项目没有改为 MIT，也未发布 crate。公开快照与审查基线见 [公开说明](docs/PUBLICATION.md)。[第三方说明](docs/third-party-notices.md) 不替代各依赖的原始许可。

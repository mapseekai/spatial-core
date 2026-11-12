# 独立验证

## 默认检查

```sh
bash scripts/ci/check.sh
```

要求 Rust 1.98.1（rustfmt、Clippy）、Python 3.11+、GDAL 与 libclang。默认测试使用内存或临时目录生成数据；`testdata/points.geojson` 是三个人工点，不含用户文件。示例临时文件会自动删除。

`.github/workflows/ci.yml` 定义 Ubuntu 24.04 的 GDAL/PostGIS 全套检查和 macOS 默认检查。远端检查须以 [GitHub Actions](https://github.com/mapseekai/spatial-core/actions) 中对应提交的实际结果为准，不能仅凭工作流定义声称通过。GDAL 绑定 0.19 支持 3.4+，CI 的系统包决定实际版本，驱动列表和原生数据仍需匹配。

GeoParquet 需要 GDAL Parquet driver。当前默认回归不会证明这个可选原生驱动可用，也不覆盖真实 S3、远程 TLS PostgreSQL、服务端算子/Temporal、Windows 或生产负载。

## 真实数据库回归

只使用新建的、可丢弃的数据库。测试用户需创建 schema、表、索引与序列的权限；用数据库管理者提前创建 `postgis` 和 `postgis_raster` 扩展。测试会创建随机名称的对象，并主动测试替表、取消和失败回滚，不能指向业务库。

```sh
psql "$SPATIAL_CORE_TEST_POSTGRES_DSN" -v ON_ERROR_STOP=1 \
  -c 'CREATE EXTENSION IF NOT EXISTS postgis;' \
  -c 'CREATE EXTENSION IF NOT EXISTS postgis_raster;'
bash scripts/ci/test-postgis.sh
```

脚本要求 DSN 并运行全部 14 个 ignored database tests，原本依赖外部手工数据的测试已改为合成 fixture。未配置 DSN 或缺扩展必须失败。默认 `cargo test` 的 14 ignored 不属于数据库验证成功。

主要覆盖：bigint ID 超出 int32、integer[] 空数组/NULL、原始字段名和类型、空矢量/Z、CRS 重投影、失败回滚/重试替换、源 ID 冲突、词法游标索引、查询取消后连接复用，以及栅格 producer 失败/取消保留已提交数据。另两项使用 GDAL GeoPackage + SQLite JSON 表达式在首次/中途读取时触发真实原生错误，验证不能把失败当作 EOF/空输入，并检查保留已提交表及正常重试。

## 复现与依赖

`Cargo.lock` 从 MapSeek 已锁版本裁剪，不依赖相邻源码或源工作区的 feature 合并。使用 `--locked`；离线环境可再加 `--offline`，前提是依赖已经在 Cargo cache。首次安装 native 工具或下载依赖需按运行环境权限处理。

macOS 的 Homebrew GDAL 和 PostGIS 必须链接相容的 native 库。若安装了多个 GDAL keg，不要为测试擅自重写全局 symlink；使用独立测试进程或容器的匹配环境。测试数据库和目标目录应独立，不复用用户生产数据库。

本次实测详情见 [提取基线验证记录](VALIDATION.md)，区分默认测试、数据库测试、独立消费者检查与未执行项。

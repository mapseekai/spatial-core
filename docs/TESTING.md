# 独立验证

## 默认检查

```sh
bash scripts/ci/check.sh
```

要求 Rust 1.98.1（rustfmt、Clippy）、Python 3.11+、GDAL 与 libclang。默认测试使用内存或临时目录生成数据；`testdata/points.geojson` 是三个人工点，不含用户文件。示例临时文件会自动删除。

`.github/workflows/ci.yml` 定义 Ubuntu 24.04 的 GDAL/PostGIS 全套检查和 macOS 默认检查。远端检查须以 [GitHub Actions](https://github.com/mapseekai/spatial-core/actions) 中对应提交的实际结果为准，不能仅凭工作流定义声称通过。GDAL 绑定 0.19 支持 3.4+，CI 的系统包决定实际版本，驱动列表和原生数据仍需匹配。

GeoParquet 需要 GDAL Parquet driver。当前默认回归不会证明这个可选原生驱动可用，也不覆盖真实 S3、服务端算子/Temporal、Windows 或生产负载。本次独立 TLS 实测单列于验证记录，不是默认 CI 的一部分。

## 真实数据库回归

只使用新建的、可丢弃的数据库。测试用户需创建 schema、表、索引与序列的权限；用数据库管理者提前创建 `postgis` 和 `postgis_raster` 扩展。测试会创建随机名称的对象，并主动测试替表、取消和失败回滚，不能指向业务库。

```sh
psql "$SPATIAL_CORE_TEST_POSTGRES_DSN" -v ON_ERROR_STOP=1 \
  -c 'CREATE EXTENSION IF NOT EXISTS postgis;' \
  -c 'CREATE EXTENSION IF NOT EXISTS postgis_raster;'
bash scripts/ci/test-postgis.sh
```

脚本要求 DSN 并运行全部 ignored database tests，测试数量由源码决定。未配置 DSN 或缺扩展必须失败。默认 `cargo test` 的 ignored 项不属于数据库验证成功。测试使用随机 schema 和合成 fixture，并在正常退出时删除。

主要覆盖：

- vector：bigint identity、精确 decimal、微秒/时区、NULL/空数组、文本转义、字段改名与大小写、空表/Z/M/ZM/EMPTY、坐标数值重投影、类型恢复失败回滚。
- source：GeoPackage + SQLite JSON 表达式在首次/中途读取时触发原生 GDAL 错误，验证失败不能变成 EOF 或空源。
- raster：7 种像元类型、NaN/缺失 NoData、多波段、skew、边缘窗口；InDB/OutDB × Whole/Tiled 的真实像元读回、catalog 约束与异构 metadata。
- transaction：并发 writer 按目标串行化、不同表可继续写入、锁超时、pool max_size=1、取消与回滚、保留既有约束、继承/分区/RLS 拒绝；COMMIT 边界取消、延迟约束拒绝与终止测试 backend 后的未知结果。
- connection：事务内扩展 schema 解析、连接设置恢复、取消后连接丢弃。
- 2026-11-13 修复回归：精细分辨率/NoData 和 raster 重投影；vector/raster 在单 blocking 线程和冷 localhost 池下完成，排队 reader 取消不等待无关 blocking 任务；普通、延迟及 DDL event trigger 改变行数时回滚、改写几何时报告最终范围；ReplaceRows 逐行精确检查 raster header，包括手工无约束表；metadata 与外部 DROP/CREATE 竞争；取消连接单独失败时及时丢弃连接；ZIP 根、嵌套目录及明确成员；文本浮点精度拒绝与旧行保留；混合 SRID 与边缘 tile 的范围区别。

协议故障测试使用本机临时 TCP 代理，专用 DSN 必须指定 TCP host（可用 127.0.0.1）；测试会暂停指定 SQL 或拒绝新增取消连接，只转发到该专用库。它们不依赖真实网络故障或固定睡眠猜测执行阶段。

DDL event trigger 回归需要 PostgreSQL 超级用户权限，只匹配本次随机 schema 的目标表，结束后删除。

数据库测试用户还需要设置测试会话的 `postgis.enable_outdb_rasters` / `postgis.gdal_enabled_drivers`，以及终止同一测试用户所创建 backend 的权限。OutDB 测试要求 PostgreSQL 能从**相同绝对路径**读取合成 TIFF；仅能连接远端数据库不够。CI 将 `/tmp/spatial-core-fixtures` 只读挂入服务容器，数据库步骤把 `TMPDIR` 指向该目录；仅合成 raster fixture 设置为 0644。不会为了让测试通过而修改真实输入文件权限。

公开示例也使用同一个专用 DSN，创建独立随机 schema 并在结束后清理：

```sh
cargo run --locked --example postgis_vector
cargo run --locked --example postgis_raster
```

TLS 不作为所有测试的隐式要求。需要额外验证时，使用调用方的 TLS crate 构建一个独立消费者，将**同一个 connector** 传给 deadpool Manager 和 `PostGisClient::from_pool`。以 `sslmode=require`、可信 CA 和正确主机名连接，查询 `pg_stat_ssl` 确认加密；在观察到 writer 等锁后取消，断言回滚、旧数据及新 backend PID。不得以关闭证书校验代替此验证。本次使用 `postgres-native-tls 0.5.3` + `native-tls 0.2.18` 和临时 CA，未向库添加 TLS 实现依赖。

## 复现与依赖

`Cargo.lock` 从 MapSeek 已锁版本裁剪，不依赖相邻源码或源工作区的 feature 合并。使用 `--locked`；离线环境可再加 `--offline`，前提是依赖已经在 Cargo cache。首次安装 native 工具或下载依赖需按运行环境权限处理。

macOS 的 Homebrew GDAL 和 PostGIS 必须链接相容的 native 库。若安装了多个 GDAL keg，不要为测试擅自重写全局 symlink；使用独立测试进程或容器的匹配环境。测试数据库和目标目录应独立，不复用用户生产数据库。

重构后的实测详情见 [PostGIS 验证记录](POSTGIS_VALIDATION.md)。[提取基线验证记录](VALIDATION.md) 保留当时事实，不作为当前 API 的测试结果。

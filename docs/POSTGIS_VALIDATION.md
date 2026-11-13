# PostGIS 重构验证记录

首次验证日期：2026-11-12；最新修复验收：2026-11-13。对象为本次未提交工作区，重构前基线为 `dab0024d712453dd02fb1aebac80ac765aaa72ab`。本记录保留首次执行事实；设计与迁移要求见 [POSTGIS_REFACTOR](POSTGIS_REFACTOR.md)，重跑方法见 [TESTING](TESTING.md)。

**当前状态：后续审查确认的 8 项问题已修复，复审追加的组合场景也已修复。** 最终检查通过 37 项默认测试、24 项专用数据库回归和 2 项 Rustdoc 测试；26 组桌面数据导入得到 23 组成功、3 组契约拒绝、0 组意外失败。两位独立 agent 的最终复审均无剩余已确认阻断项。源码、测试对应关系和完整证据见 [修复与最终复审报告](reviews/2026-11-13-postgis-fixes.md)。以下 32/15 项及 TLS 结果属于首次验收，不作为本轮重复执行的声明。

## 实施结果

数据库入口统一为 `postgis::vector::PostGisVectorWriter` 与 `postgis::raster::PostGisRasterWriter`。两者共用连接、事务、字节背压、取消和提交结果处理，各自保留 source/schema/table/空间编码/metadata。旧 `pg` 与重复导入路线已删除，文件转换留在顶层 vector/raster。

这次按用户授权实施破坏性迁移：默认 Create、显式覆盖模式、显式 GDAL 配置、精确 schema 和类型化错误。没有维护旧 API facade。没有修改相邻 MapSeek 消费者、许可或发布设置，也没有提交或推送。

## 首次验收通过的检查（2026-11-12）

| 检查 | 结果与边界 |
| --- | --- |
| `bash scripts/ci/check.sh` | 结构检查、rustfmt、全部 targets 的 Clippy `-D warnings`、32 项默认测试、2 项可编译 Rustdoc、Rustdoc 构建、转换示例及 diff 空白检查通过 |
| `bash scripts/ci/test-postgis.sh` | 15 项真实数据库集成测试通过；默认测试中的 ignored 没有计入这一结果 |
| `cargo run --locked --example postgis_vector` | 创建随机测试 schema，提交并核对 3 个 EPSG:3857 feature，随后清理 |
| `cargo run --locked --example postgis_raster` | 创建合成 TIFF，提交 4 个 tile，实际读回像元总和 100，随后清理 |
| 独立 TLS 消费者 | 新的 Cargo 项目与独立构建目录；`sslmode=require`、临时 CA、有效 serverAuth/SAN，证书链和主机名校验开启；确认 `pg_stat_ssl`、导入、锁等待取消、旧行保留及取消连接被丢弃 |
| 非 public 扩展 | 独立测试数据库，将两个扩展安装到 `geospatial`，连接初始 `search_path=pg_catalog`；矢量替表/替行、栅格创建及 metadata 查询通过。仓库回归另断言事务结束后原 search_path 恢复 |
| 依赖变更 | Cargo.lock 未增加或升级包版本，移除 42 个不再使用的锁定包；UUID 仅保留为开发依赖。TLS 实现只用于临时消费者，没有加入库依赖 |

测试环境为 macOS arm64、Rust 1.98.1、客户端 GDAL 3.12.2、PostgreSQL 17.10、PostGIS/postgis_raster 3.6.4。数据库进程使用其匹配的 GDAL 3.13.1、GEOS 3.14.1、PROJ 9.8.1。为匹配已有 Homebrew keg，只对临时数据库进程设置库路径，没有改全局 symlink 或 Cargo 配置。

数据库为本任务创建的临时集群，监听随机本机端口，使用可丢弃数据库。OutDB 的服务器与客户端在同一台机器上读取合成 TIFF；这不证明真实 S3 或跨机器文件引用可用。TLS 消费者使用 `postgres-native-tls 0.5.3` 与 `native-tls 0.2.18`，没有修改系统信任库。

验收结束后已停止并删除本任务的临时数据库集群、测试 CA 私钥及临时消费者构建目录。

## 首次验收的数据与故障覆盖

| 原保障 / 本次新增契约 | 当前测试归属与关键断言 |
| --- | --- |
| 原 vector COPY、identity、字段恢复 | `postgis/vector/writer.rs`：identity 超出 int32，30 位 decimal 不经过 f64，数组 NULL/空数组、控制字符、大小写、额外 nullable 列、精确微秒和 offset；ReplaceRows 保留精确类型，转换失败回滚 |
| 原空矢量与几何契约 | 同文件：显式空表、自定义 geometry 列、Z/M/ZM、全 EMPTY 的空 extent；Shapefile line/polygon 归一为 multi 并保留 Z/M，EPSG:3395 目标，以及无 authority WKT 的实际坐标重投影 |
| 原首次/中途源读取失败 | 同文件及 `gdal_ext/ffi.rs`：真实 SQLite/GDAL 错误不变成 EOF，原已提交表保留，随后正常重试 |
| 原 raster 入库 | `postgis/raster/writer.rs`：InDB/OutDB × Whole/Tiled、多波段、skew、边缘 tile、所有 7 种像元类型、NaN 和缺失 NoData、真实 ST_Value 读回、catalog 检查、旧约束拒绝和取消回滚 |
| 目标对象保护 | `postgis/vector/writer.rs`：两类替换都拒绝继承父子表、分区父子表及 RLS 目标，并核对旧数据仍存在 |
| 连接与同目标竞争 | `postgis/transaction.rs`：两个 writer 等待同一 advisory lock 时，另一个目标仍可完成；释放锁后两次替换完成且没有追加。其余数据库 fixture 使用 max_size=1 |
| COMMIT 边界 | 同文件：Committing 回调收到取消后仍等待真实提交；延迟约束拒绝返回 Database/Committing；提交期间终止测试 backend 返回 CommitOutcomeUnknown，随后连接 PID 改变 |
| 生命周期与资源 | `postgis/stream.rs`、两类编码模块及 `raster/tiling.rs`：worker 错误/panic 不算 EOF，丢弃流唤醒等待字节许可的 worker，计数上限、转义前预算、超大 VRT 惰性拒绝、overlap/边缘窗口 |
| 配置与扩展 | `config/gdal.rs`：正常退出和 unwind 恢复线程原值，Debug 不输出选项值；`postgis/connection.rs`：事务内扩展解析与调用方 search_path 恢复 |

原测试按职责重新归属并合并，不以原测试函数数量作为覆盖率。旧配置缓存、自动字段改名、强制 4326/3857 等已明确改变的行为，不继续作为新实现的兼容断言。

## 代码复查及已修复问题

按 rust-skills 检查公开类型、错误链、线程所有权与取消边界；按 ponytail-review 检查完整调用链、失败回滚、实际数据与不必要的重复实现。负载假设为库被多个导入请求共用，单次写入一个普通表；未把功能回归当成生产吞吐验证。

1. **无 NoData 的宽像元 WKB 错误。** `wkb-raster 0.2.1` 固定写一个占位字节，宽像元需要完整宽度，原行为导致 COPY 报 WKB 截断。适配层按波段宽度补齐，并接受已经完整的编码；混合波段偏移单测和真实 InDB/OutDB 回环验证了修复。
2. **扩展 SQL 的内部名称解析。** 外层函数限定 schema 仍不足以处理 PostGIS 内部未限定的类型。已在事务内配置已确认的扩展 search_path，提交/回滚后恢复；非 public 数据库实测通过。
3. **精度与空值。** 拒绝不能精确表示的大整数到浮点转换；字段读取先检查 unset/NULL，避免 GDAL 默认值替代空值；保留时间文本后再验证目标精度，原生 DateTime 暂明确拒绝。
4. **内存边界。** 属性转义在扩张行缓冲区前检查预算，整数数组构造也受预算约束。栅格窗口仍在读取前做峰值估算；这些预算不承诺限制 GDAL 缓存或整个进程 RSS。
5. **提交与取消。** COMMIT 发出后不再与取消竞速；FATAL/断线不假定回滚；取消连接始终丢弃，避免迟到的取消包影响池中下一次查询。

栅格各像元类型的重复读取收敛为一个带类型的私有函数；writer 中的 DDL/约束计划移入各自 `table.rs`。没有引入公共通用 Source trait、额外连接池、Append 占位、插件框架或第二条 COPY 路线。

2026-11-12 的就绪判断随后被 [2026-11-13 审查报告](reviews/2026-11-13-postgis.md) 的 2 项 P1、6 项 P2 取代；这些发现现已全部修复，当前就绪判断以 [修复与最终复审报告](reviews/2026-11-13-postgis-fixes.md) 为准。问题与验证保存在本地记录，未创建外部跟踪事项。

## 未验证与使用边界

- 未运行远端 CI 或 Windows。首次验收没有使用 Linux 服务容器；2026-11-13 后续验收使用 Docker 中的 Linux PostgreSQL/PostGIS，Rust 客户端仍在 macOS arm64，不能据此声称 Rust 客户端经过 Linux 验证。
- 未验证真实 S3、远程 OutDB 路径、长期凭据轮换、海量导入吞吐、峰值 RSS 或长期压力；没有性能提升倍数结论。
- `ReplaceTable` 重建表对象且整个导入持排他锁；`ReplaceRows` 保留旧约束，输入不匹配会回滚。旧快照、TRUNCATE 和新表可见性仍遵循 PostgreSQL MVCC 限制。
- 原生 OGR DateTime、GeometryCollection/曲线、独立 mask、非默认 scale/offset、无仿射的 GCP/RPC、mosaic/overviews/Append 不在当前输入契约内。
- 直接 abort/drop future 无法同步等待原生 GDAL I/O 停止；需要确认清理完成时应取消 token 并继续 await。CommitOutcomeUnknown 必须由调用方核对后再决定重试。
- 外部消费者须按 [迁移表](POSTGIS_REFACTOR.md) 更新。此次未修改它们，也未提交、推送或发布。

# DuckDB 验证记录

日期：2026-11-18。基线 `49c642410326eae115faf47e13abe71b700ae006`，加尚未提交的 DuckDB 改动。本文不是远端 CI 或生产验收结果。

## 原生修复后的验证

修补并源码构建 Spatial 后，GDAL 多格式 reader 已恢复。Session 显式固定公钥并从稳定路径加载；未开启 unsigned。原生错误、部分批次、metadata 选项和列名冲突由 Spatial 补丁处理，FlatGeobuf NULL 位图由 GDAL 补丁处理，旧版官方 Spatial 的 GDAL reader 仍被拒绝。原生源码、版本和构建步骤见 [补丁说明](../third_party/duckdb-spatial/README.md)。本次实现后新增六轮双人审查，A 六轮、替换 B 五轮；R6 均无未关闭确认缺陷。之前的历史四轮和实现 helper 不计入这些轮次。

- macOS arm64 / Rust 1.98.1 / 系统 GDAL 3.12.2；DuckDB core `5766c07b594e344320d13d5ad4f4681f2737f338`（v2.0.0）；Spatial `a3dbcd1781ec26c3a1be9bd622be05b376d6f47a` 加补丁。
- 在新的临时源码/构建目录编译 GDAL 3.12.4、GEOS 3.14.1、PROJ 9.1.1 等 vcpkg 固定依赖及静态 DuckDB core，再链接扩展。没有借用已有 native 二进制规避源码构建。构建日志保留上游 deprecated API 和重复链接库 warning，未计为零警告构建。
- 新构建脚本完整运行一次，复用本轮刚生成的中间产物，得到相同字节的签名构件；这不是第二次从空目录编译。签名程序另验证无覆盖和无效 footer 拒绝。
- R2 发现 FlatGeobuf NULL 缺陷后，通过构建脚本重新编译 GDAL Debug/Release 并重新链接、签名 Spatial；R4/R5 两次列名修复后，分别再次编译 Spatial 并链接签名。核心和其他固定依赖复用本轮已构建产物。当前扩展 `78302398` 字节，SHA-256 `41f44ad005b78fa2cfc4dd8ce26bbe254ee7bb7ff99e1db495e6fab890d2d2ce`。
- Spatial/overlay 补丁 SHA-256 `f8b5f08a5f6876c299140be1ab067761c51f50903cada181c0795664bb7c8a55`；GDAL 补丁 SHA-256 `9a6b4b10b8b8bccb12396a52da29b21924dc81b51ca1b6061f8a0df69e74b76a`。
- 当前构件与清单：`/private/tmp/spatial-core-patched-extensions-20261118-r6`。签名公钥为本轮测试生成；私钥不进入源码、部署目录或审查归档。此前 `-r3` 和 `-r5` 构件保留为两次列名失败对照，不能用相同版本号代替构件哈希。

| 实际检查 | 结果 |
| --- | --- |
| 独立快照 `bash scripts/ci/check.sh` | fmt、Clippy、102 个测试、4 doctest、doc、4 个基线示例通过；26 ignored 不计通过 |
| 独立快照 `bash scripts/ci/test-duckdb.sh` | R6 构件：feature Clippy、签名预装、109 个普通测试、4 doctest、25 个显式原生测试、doc、GeoJSON→空间 SQL→GeoParquet 示例通过 |
| 共享工作区 `bash scripts/ci/check.sh` | R6 源码：fmt、Clippy、147 个普通测试、4 doctest、doc、7 个示例通过；26 ignored 不计通过 |
| 共享工作区 `bash scripts/ci/test-duckdb.sh` | R6 构件：feature Clippy、签名预装、154 个普通测试、4 doctest、25 个显式原生测试、doc 与 DuckDB 示例通过 |
| 本地格式 | GeoParquet、GeoJSON、FGB、GPKG、Shapefile、KML、GML：行数、坐标和 CRS；CSV WKT：行数与坐标 |
| HTTP/S3-compatible fixture | 上述除 CSV 外的七种格式，公开 HTTP、Authorization HTTP、静态 S3 凭据均读回 (120,30)；旁文件实际可达 |
| GDAL 失败 | FGB 截断、GPKG B-tree 页损坏、Shapefile 截断均报错且无输出发布；VRT SQL 在不同有效行数切点失败均报错；独立审查另确认 schema 成功后的 Scan 失败；随后正常 GDAL 读取成功 |
| NULL/EMPTY | 4097 行全 NULL 与混合 FGB，系统 GDAL 独立验证 fixture；跨批次、raw WKB NULL、空间过滤与 GeoParquet 往返通过。完全相同回归在旧构件上因 Unexpected end of binary data 失败 |
| 列名冲突 | CSV 保留 WKT、GeoJSON 属性与几何重名、大小写与已占 `_N` 后缀、raw WKB、普通非空间属性及 GeoParquet 往返通过；新回归在 R3 构件上因 duplicate column name 失败 |
| 空属性名 | 空名保留原有 C<index> 默认名；与现有 C2/c2 和后缀冲突时，metadata、默认/raw 读取、属性值、坐标及 GeoParquet 往返通过。新回归在 R5 构件上失败，在 R6 通过 |
| metadata/空源 | GPKG LIST_ALL_TABLES=NO/YES、显式图层、错误 driver、Shapefile sibling `.prj` 一致性；真正空 GeoJSON 仍成功输出零行 |
| 安全与生命周期 | 签名尾部被改动时加载失败并清理 scratch；保留签名检查、无自动安装、凭据隔离、重定向拒绝策略、取消与输出保护回归 |

日志位于 `/private/tmp/spatial-core-native-{configure,configure-final,build,install,default-check,feature-check,script-check}.log`。非 UTF-8 路径测试在沙箱内收到 EPERM 后，在沙箱外重跑默认检查通过；首次原生测试中的两项失败来自 fixture COPY 选项语法，修正后 22 项全部通过，没有将这些初次失败计为通过。

共享工作区另有并行 GDAL 开发。本轮早期共享 feature Clippy 被 `raster/overviews.rs` 的两处未使用 Report 挡住；随后默认检查停在 `raster/calculate.rs:348` 的 `raster_algebra_rejects_dataset_nodata_tuples_before_writing`，146 passed、1 failed、26 ignored。这些失败未计通过，相关源码也未由本任务修改。并行改动更新后的共享默认与 feature 检查均完整通过，结果见上表；这不表示已审查并行 GDAL diff。实际原生测试通过不等同于真实 AWS、完整 HTTPS/TLS、Linux、远端 CI、生产负载、磁盘耗尽或专用 PostGIS 数据库验收；这些尚未执行。

当前检查日志位于 `/private/tmp/spatial-core-native-review-20261118`：`round-3-native-build.log`、`round-3-default.log`、`round-3-feature.log`、`round-3-null-after.log`，旧构件对照为 `round-3-null-control-old-build.log`。新 feature 检查包含 7 项离线路径回归和 GeoJSON/FGB 重定向用例，109 个普通测试及 4 个 doctest 通过；49 ignored 中的 23 项 DuckDB 测试随后显式执行通过，其余 26 项未执行。

列名修复后的日志为 `round-5-native-build.log`、`round-5-install.log`、`round-5-columns-after.log` 与 `round-5-feature.log`；共享完整检查为 `round-5-workspace-default.log` 和 `round-5-workspace-feature.log`。两次完整 feature 检查的 50 ignored 中，24 项原生测试随后显式执行通过，其余 26 项未运行。修复前对照为 `round-4-columns-before.log` 和 `round-4-columns-regression-old.log`。列名修补由独立实现 agent 在私有目录提出，主 agent 审阅、应用并构建验证；该实现 agent 不计入 A/B 独立审查轮数。

空名修复的实际对照为 `round-5-empty-name-before.log`、`round-5-empty-name-regression-old.log` 和 `round-6-empty-name-after.log`；新构件构建/安装记录为 `round-6-native-build.log`、`round-6-install.log`。独立完整检查为 `round-6-feature.log`，共享完整检查为 `round-6-workspace-default.log` 和 `round-6-workspace-feature.log`，均完整通过。两次 feature 检查的 51 ignored 中，25 项原生测试随后显式执行通过，其余 26 项未执行。第二位独立实现 helper 只提供了私有目录中的五行 native delta，主 agent 审阅、应用、重建并验收；该 helper 同样不计入 A/B 审查。

共享检查日志为 `round-3-workspace-default.log` 与 `round-3-workspace-feature.log`。后者使用字节一致的私有部署副本。首次复制连带安装缓存，其旧绝对路径被安装器拒绝；清除该测试副本的安装缓存后，正常安装及完整检查通过，原部署未改动。首次失败保留为 `round-3-workspace-feature-copy-origin.log`，不计通过。

R2 完整重跑曾出现两个本机 HTTP fixture 失败。实际 fd 标志和传输诊断确认 macOS accept 继承 O_NONBLOCK，恢复 blocking 后 9 项 source 回归和新构件完整检查均通过。NULL/EMPTY 新测例首次生成时发现 GDAL 的 FGB writer 会将 EMPTY 写成 NULL，因此在既有 Point buffer 内编码 NaN 坐标，并先由系统 GDAL 读回确认 NULL/EMPTY 数量；没有通过放松预期值掩盖差异。初始 raw WKB 列名与 GDAL geometry() 用法也已修正，采用 geometry_by_index 的 NULL 检查。失败的 fixture 尝试不计通过。

最终 R6 冻结指纹为 `b18e36a0adebc62d26bdf4d9dd094a1c56ecce0277e93f370007117178d19785`。两位独立审查的共享/私有副本均 29/29 文件匹配，原始反例文件和实际构件哈希不变；审查结束后仅补充两份结果文档，源码与已测试实现不变。[本地归档](../artifacts/reviews/2026-11-18-duckdb-native/README.md) 保留每轮清单、报告、失败对照及最终检查日志，不包含私钥和二进制。

当前 Ponytail 结论：已确认问题均已修复，使用既有 DuckDB 命名与 Arrow 所有权结构；没有增加统一引擎 trait、任务层或自动后端回退。Fix Root Causes 对应原生读取边界及命名顺序的修补，Prove It Works 对应同一反例的旧/新构件对照和两位独立运行验证。没有提交、推送或发布。

证据边界仍包括真实 AWS/HTTPS/TLS、Linux/其他架构、远端 CI、生产规模与性能、ASan/UBSan/泄漏、allocator/fatal 故障、磁盘耗尽和清理权限失败。原生 GDAL 故障发生在 Rust 已消费前缀之后，以及 NULL 空间过滤和 geometry-ignored 分支，仍无动态覆盖证明。原生驱动接受但不报告的格式问题不由本补丁充当完整格式校验器。专用 PostGIS 数据库路径未改动，也未在本次执行；类型化算子 API 和直接 Arrow→PostGIS 适配仍属后续阶段。

## 以下为原生修复前的历史记录

此前四轮审查通过禁用 GDAL reader 阻断错误，以下结论只针对当时版本。原始缺陷保留作修复前证据，不代表当前修补版本仍禁用 reader。

## 构建与环境

- macOS arm64，Rust 1.98.1，系统 GDAL 3.12.2。
- `duckdb` Rust binding 1.20000.2，实际 bundled 引擎 v2.0.0，Arrow 60.1.0。
- Spatial/httpfs 从官方 v2.0.0/osx_arm64 HTTPS 构件重新下载到独立临时目录，加载时保留原生签名/版本检查；最终验证不依赖用户的 DuckDB 扩展缓存。
- Spatial 探测到 48 个 GDAL 驱动；这不是系统 GDAL 的驱动列表。没有假定两套 GDAL/PROJ/GEOS 版本相同。
- 独立验证目录 `/private/tmp/spatial-core-duckdb-sgv1he3k` 从上述基线导出，仅复制本次 Cargo、条件导出、DuckDB 模块、示例和检查脚本。在空 target 中重新编译 bundled 引擎；没有引用相邻 MapSeek 源码或预存 native 构建产物。
- Cargo.lock 增加可选依赖所需包，既有 package 的 name/version 没有删除或升级；默认 `cargo tree -e normal --depth 1` 无 DuckDB/Arrow 直接依赖。

实际下载构件 SHA-256：

| 文件 | 字节 | SHA-256 |
| --- | ---: | --- |
| `spatial.duckdb_extension` | 80499742 | `212ca75890f576bec4a9ea71f5a10320581fe9886afceb05b1d47944ebcf03f4` |
| `httpfs.duckdb_extension` | 19679534 | `c2d72a98ad081bf4d62c908b8d9f60b70ab1b83faa9141fc7e7d53526658761d` |

## 旧四轮审查后的检查

两位审查者各完成四轮，共八份正式报告；第四轮均无新增确认阻塞项。七项库边界问题已修复；GDAL 原生错误传播和打开选项 metadata 问题随 reader 禁用而阻断。见 [逐轮证据](DUCKDB_REVIEW.md)。

| 范围与命令 | 实际结果 |
| --- | --- |
| 独立快照 `bash scripts/ci/check.sh` | 通过：fmt、Clippy、102 个普通测试、4 doctest、doc、4 个基线示例；26 ignored 不计通过 |
| 独立快照 `bash scripts/ci/test-duckdb.sh` | 通过：feature Clippy、109 个普通测试、4 doctest、18 个显式 DuckDB 集成测试、doc、GeoParquet 示例 |
| 共享工作区 `bash scripts/ci/check.sh` | 通过：fmt、Clippy、127 个普通测试、4 doctest、doc、7 个示例；包含并行 GDAL 改动，但不代表本次已审查该 diff |
| 共享工作区 `bash scripts/ci/test-duckdb.sh` | 通过：feature Clippy、134 个普通测试、4 doctest、18 个显式 DuckDB 集成测试、doc、GeoParquet 示例 |
| DuckDB 新增测试 | 7 个普通单测 + 18 个需扩展的集成测试，共 25 个全部执行通过 |
| 实现一致性 | 共享目录与独立快照的 13 个实现/配置文件均匹配第四轮冻结哈希；审查后仅更新结果文档 |

feature 普通测试中的 44 个 ignored 包含之后显式运行的 18 个 DuckDB 测试；其余 26 项没有计入通过。未运行专用 PostGIS 数据库测试。

日志保存在 `/private/tmp/spatial-duckdb-review-20261118-0mlbkhjn/`：`round-4-default.log`、`round-4-feature.log`、`round-4-workspace.log`、`final-workspace-feature.log`。第四轮源码清单和归档为 `round-4.json` / `round-4.tar.gz`，指纹 `8274c81c6c4a973c48b9f22882fee819640830591c49e924b4d833dce5a595f8`。

当前正向回归覆盖本地/HTTP/S3 GeoParquet、单一 Authorization、静态 S3 密钥、scope authority 边界、跨源重定向、取消后复用、消费者错误和 panic、预算、输出保护与空间 SQL。负向回归覆盖 GDAL reader 读取前拒绝、自定义 header/session token 拒绝、原生 Parquet 几何注解拒绝、嵌套几何/Interval 输出拒绝、原生 TIMETZ 递归拒绝，以及取消注册回滚与旧 view 保护。

## 首次实现检查（四轮审查之前）

| 检查 | 结果与边界 |
| --- | --- |
| 独立快照 `bash scripts/ci/check.sh` | 通过：结构、fmt、Clippy、102 个测试、4 个 doctest、doc、4 个原有示例；26 个 ignored 不计通过 |
| 独立快照 `bash scripts/ci/test-duckdb.sh` | 通过：结构、feature Clippy、108 个普通测试、4 个 doctest、10 个显式选择的 DuckDB 集成测试、feature doc、DuckDB 示例 |
| DuckDB 新增回归 | 共 16 个：6 个普通单测、10 个需扩展的集成测试；全部执行通过 |
| `git diff --check`、新脚本 `bash -n` | 通过 |
| 共享工作区 `bash scripts/ci/check.sh` | 当次在并行 GDAL 改动 `src/gdal_ext/mod.rs` 的模块排序 fmt 差异处停止；不能算整个工作区通过 |
| 远端 CI | 新增 Linux/macOS feature 工作流，尚未提交/推送/运行，不算通过 |

完整 feature 检查使用 `CARGO_BUILD_JOBS=4`、`CARGO_PROFILE_DEV_DEBUG=0`、`CARGO_PROFILE_TEST_DEBUG=0`，扩展目录为 `/private/tmp/spatial-duckdb-extensions-lxh6bx2g`。默认检查的 Git diff 步骤只读原仓库 Git 元数据、以独立目录作为 work tree；未更改用户 Git index。独立源码快照中默认与 feature 检查均运行实际 GDAL 程序，不以 mock 替代 native 调用。

## 初始功能证据（审查前的正向测试）

| 范围 | 已验证 |
| --- | --- |
| 本地输入 | GeoParquet、GeoJSON、FlatGeobuf、GeoPackage、Shapefile；坐标与行数读回，多图层显式选择，同名 view 不覆盖 |
| 远端输入 | 本地 HTTP 与 S3-compatible fixture 上的 GeoParquet/GeoJSON/FlatGeobuf；点坐标读回为 (120,30) |
| 网络/凭据 | Range 请求、无 Range 完整响应、保留签名 URL 查询参数、HTTP headers、S3 path-style endpoint、签名 credential header/临时 token、403、Parquet 截断、不同 Session 的 HTTP secret 隔离 |
| 数据契约 | 大于 2^53 的 BIGINT、DECIMAL(38,18)、纳秒时间、中文/引号标识符与路径、未知 CRS、全 NULL 几何列、多个几何列、EMPTY、零行；M 输出拒绝 |
| 常见空间 SQL | Filter 参数绑定；3857 重投影坐标；Buffer 面积与系统 GDAL 对照；边界点 Intersects 一对多；LEFT JOIN 零匹配；按掩膜 Clip 面积；Dissolve 面积 |
| 失败/资源 | 首批前取消、预取消、消费失败、回调 panic、已经输出批次后的原生 fetch 中断、行/字节预算、错误后连接复用、已存在输出保护、失败清理临时目录 |
| native 共存 | 同一进程反复创建 DuckDB Session、加载 Spatial、系统 GDAL 读回格式和 Buffer；显式关闭与临时目录清理 |

网络 fixture 检查 AWS 签名头、scope 路径和 token，但不验证签名的密码学正确性。它不能替代真实 AWS S3、IAM 或生产对象存储测试。原生 fetch 故障用首批回调后的 `InterruptHandle` 注入，证实错误不会变成正常 EOF；表达式 `error()` 可能在首批前暴露，不能用它假装已经验证了中途 fetch 失败。

## 实测发现与修正

DuckDB 2.0 原生 V1 COPY 对下列结果自动生成的 `geo` metadata 只包含 `unknown`，而且省略了它的 CRS：

```sql
SELECT 'POINT EMPTY'::GEOMETRY AS unknown,
       NULL::GEOMETRY('EPSG:3857') AS known;
```

读回时 unknown 被解释为默认 CRS84，known 变成没有 geometry metadata 的二进制列。存在文件/行数正确并不足以判断写入成功。

本次改为通过原生 Parquet COPY 写标准 WKB，并显式构造 GeoParquet 1.1 metadata：全部顶层几何列、完整 CRS、未知 CRS 为 null。`geometry_types: []` 和无 bbox 是规范允许的未知统计，不伪造类型或范围。发布前再比较实际 schema/CRS；空文件和全 NULL 几何列都已回归。没有添加另一套 Parquet writer 或全量 Rust 收集路径。

另外在审查中收紧了启动失败的临时目录清理：显式关闭目录，保留主错误与清理错误；输出已发布后的清理失败用独立 `PublishedCleanup` 变体说明状态。扩展下载、全局环境和现有 GDAL/PostGIS API 没有被运行时修改。

当前没有可用的 Multica 工具/项目绑定，因此上述上游行为与复现保留在本记录，未创建外部 issue。

## 历史 Ponytail review（原生修补前）

What this change does: adds an opt-in GeoParquet analysis backend with explicit credentials, streaming Arrow consumption, cancellation-safe registration, and checked no-replace publication.

Must fix: 当前启用范围在第四轮没有剩余确认阻塞项。GDAL 多格式路径仍不可交付；恢复前必须完成原生失败传播和带选项 metadata 的验收。

Should fix: 未发现当前范围内新增确认事项。真实云服务、其他平台及故障注入仍未验收。

Nice to have: 未加入统一引擎 trait、类型化算子、PostGIS 流式桥接或性能扩展。

Verdict: 当前 GeoParquet 闭环通过本地验证；最初的 GDAL 多格式目标尚未完成。未提交、推送或发布。

使用 `rust-skills`、`rust-pro`、`rust-async-patterns`、`coding-standards` 检查了依赖/条件导出、所有权、错误传播、阻塞线程边界、取消监听生命周期、参数引用、凭据日志、输出保护以及测试与部署入口；使用 `ponytail-review` 复核实际改动与相邻调用。未增加引擎统一 trait、任务/DAG 层、异步 runtime、自动后端回退或依赖升级。

## 原生修补前记录的验收边界

未执行真实 AWS S3、完整 HTTPS/TLS 失败与重定向矩阵、远端 GPKG/Shapefile、GeoArrow native encoding/GeoParquet V2、动态 epoch、全部 Z/M 算法、磁盘耗尽与清理权限故障、生产数据规模/峰值 RSS/请求字节、Linux 执行和远端 CI。取消延迟受 native I/O 影响；buffer-manager limit 不是 RSS 硬上限，输出字节预算也不是磁盘硬配额。

输出要求本地同文件系统 hard link 支持，不覆盖目标，不承诺断电持久化。SQL 只面向可信调用方；空间单位、CRS 与维度仍由 SQL 调用者明确。当前没有 PostGIS 事务路径改动，因此没有把未运行的专用数据库测试计为通过；直接结果流桥接仍未实现。

## 默认 Cargo Debug 分配器修复（2026-11-19）

旧签名构件在全新 Cargo target、默认 Debug profile 下运行 `spatial_sql_defines_boundary_clip_dissolve_buffer_and_transform_semantics`，于 `ST_Union_Agg` 的 arena 析构触发 `allocation_count >= size`，进程以 SIGABRT 退出。LLDB 另行观察到：Spatial 内嵌 Release core 使用主程序的 Debug allocator 分配 4096 字节，分配前后计数没有增加。问题来自两个模块对同一分配器记账不一致。

修复让 Release 侧的 allocate/free/reallocate 在 `debug_info` 存在时维护计数，保持 Debug 侧断言、公开布局和 Release 分配器的初始化行为。Session 使用链接引擎的实际分配器状态检查 allocator contract 1；旧构件在 Debug 下返回 `Unsupported`。固定版本的源码、精确补丁和许可见 [原生补丁](../third_party/duckdb-spatial/README.md)。

本机 macOS arm64 的新签名 Spatial SHA-256 为 `a97d471046ce82c581616bddc1669152bb50f5c93136959543d961b212db76cb`。此构件重新编译了内嵌 DuckDB 与 Spatial，复用了先前构建的固定版本 vcpkg 依赖；本地结果不等同于远端冷构建。

- 原始失败用例在默认 Cargo Debug 下通过，未设置 `CARGO_PROFILE_DEV_DEBUG` / `CARGO_PROFILE_TEST_DEBUG`，未关闭断言。
- 使用同一 Rust 修复和旧 r6 签名构件时，打开 Session 返回缺少 allocator contract 的错误，未发生 SIGABRT。
- 独立双动态库探针编译真实 allocator 源码，修补前复现断言；修补后通过 96 组模块/归属/私有数据组合，包含 grow/shrink realloc、RAII、空指针与跨模块析构。探针使用简化异常格式化实现，只作为计数路径证据。
- 自定义 `DUCKDB_DEBUG_ALLOCATION` 的额外指针映射不受支持；混合模式探针仍会触发指针映射断言，因此本库在检测到该编译宏时明确拒绝。

独立消费方在另一个全新 target 中编译，manifest 不含 Cargo profile 调整，使用声明的 Rust 1.98.1。默认 Debug 下连续八个 Session 的 `ST_Union_Agg` 面积均为 6，关闭后临时目录清空。第二个消费方只对 `libduckdb-sys` 设置 package `debug = 0`，Rust 自身仍启用 `debug_assertions`；新补丁、旧补丁和官方扩展均完成同样八次查询。默认 Debug 消费方对旧补丁和官方扩展则正常返回 `Unsupported`，退出码为 1，没有 SIGABRT。

默认门禁通过 147 项库测试、4 项 doctest 和 7 个 GDAL 示例。DuckDB 门禁通过 155 项常规测试、4 项 doctest、25 项原生集成，包含格式检查、Clippy、rustdoc 和示例。默认检查首轮的非 UTF-8 路径测试受沙箱 EPERM 限制；在沙箱外重新执行完整门禁后通过，未改测试以规避限制。PostGIS 与系统 GDAL 可选 GeoParquet 忽略项不计入本次本地结果。

审查还验证了两个边界。CI 单测在设置 `SPATIAL_CORE_DUCKDB_EXPECT_DEBUG=1` 时，必须观察到实际原生 Debug 计数器；通过 LLDB 强制探针返回 false，单测按预期失败。原生扩展编译同时拒绝 `DEBUG` 和 `DUCKDB_DEBUG_ALLOCATION`，避免复用 CMake 构建目录时的 `FORCE_DEBUG`/`DEBUG_ALLOCATION` 选项绕过 Release 构件约定。普通 Debug 宿主与 Release 扩展的 128 组失败回调探针通过，包含返回空指针、抛异常、失败扩容/缩容后原数据及清理的检查；探针采用真实 allocator 源码和简化异常格式化。最终配置保护补丁重新构建后，上述二进制 SHA-256 未改变。

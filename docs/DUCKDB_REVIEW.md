# DuckDB 对抗审查

日期：2026-11-18。范围是本次可选 DuckDB 阶段 0/1 实现及原生 Spatial/GDAL 修补；共享工作区中的并行 GDAL 改动不在此次审查范围。每轮由两位审查者分别检查同一冻结快照，确认问题修复后再进入下一轮。探测仅使用本机临时文件和合成凭据。

原生错误传播修补、多格式读取与 25 项原生回归已完成。新增原生审查共六轮：A 完成 R1–R6，替换后的 B2 完成 R2–R6，分别六轮和五轮，均满足至少四轮要求。R6 两位均确认当前范围没有未关闭的确认缺陷。下面的历史审查、实现 helper 和主 agent 检查均未折算为新轮次。

## 新原生审查记录

工作日志位于 `/private/tmp/spatial-core-native-review-20261118`；每轮源码、逐文件哈希、独立报告和最终检查日志已保留到仓库本地 [审查归档](../artifacts/reviews/2026-11-18-duckdb-native/README.md)。该目录按现有项目规则被 Git 忽略，不包含私钥或原生二进制。

| 轮次 | 快照 SHA-256 | 审查结论与处理 |
| --- | --- | --- |
| 1 | `bac157cba55839e35eacde980fcda7b8ac270b3278d77b930b76f8bc5d5327ad` | A 无新增确认问题；B 发现构建输出的逻辑路径与私钥物理路径比较不一致。统一 `pwd -P` 并修正根目录边界，6 项离线路径回归通过；R2 继续补齐大小写别名 |
| 2 | `79497c4775438f54febdb15af4cb6c478c06751252781c8080dba2c9d509eb2c` | A 复核首轮修复后发现大小写路径遗漏及 GDAL FlatGeobuf NULL 几何缺陷；B2 无新增确认问题。目录检查改为 samefile，GDAL NULL 补丁已重建并通过新旧构件对照和 23 项原生回归 |
| 3 | `e1eb60ea0889cee0cd3fc40ac908c379d35ec6d44e95a47d31d50fa2b980e647` | 两位均无新增确认 P1/P2；独立验证路径别名、4097 行 NULL/EMPTY 与 8205 行变长 WKB、跨批次属性和关闭后所有权、HTTP fixture 根因及六次传输失败后复用。共享/私有各 29/29 文件及新原生哈希均匹配 |
| 4 | `8579aadf619da0bf2ca71f25e58bae25671a674b9464ec70e4e73e2cd341fe6d` | A 发现合法 CSV/GeoJSON 属性与几何列重名使 ST_Read 无法绑定；主 agent 独立复现，新回归在旧构件失败，复用 DuckDB 命名规则的原生修补已重建并通过 24 项原生检查。B2 四 Session 并发与取消/error/panic 后复用、旧 view 和输出保护通过；两位各 29/29 文件及构件哈希匹配 |
| 5 | `ae43332a9408decf861d95d47eba1f0543b7f20fbc35590fd9d3bcbff1bccc8c` | A 原始列名矩阵及 9 次完整 schema/数组往返通过；B2 独立矩阵 8 过、1 失败，确认空名在去重后才被 binder 填成 C2，与现有 C2 冲突。主 agent 已独立复现，在去重前采用相同 C<index> 规则，重建及 25 项原生检查通过；该项在 R6 独立关闭 |
| 6 | `b18e36a0adebc62d26bdf4d9dd094a1c56ecce0277e93f370007117178d19785` | 两位独立关闭空名问题，无未关闭确认缺陷。A 分进程旧失败/新通过、不同列索引及 4 次完整往返通过；B2 原始失败文件、既有九项矩阵及反向冲突边界共 11/11 通过。共享/私有各 29/29 文件与源/installed 构件哈希匹配 |

旧 B 会话 `/root/duckdb_review_b` 在完成 R1 报告后被工具系统连续中断。替换审查者 `/root/duckdb_review_b_native` 从总序 R2 开始，最终完成五轮；A 为 `/root/duckdb_review_a`，最终完成六轮。被中断的调用不计通过，两位只写私有探针与报告，未实现被审查修补。

R2 两份私有冻结副本均 27/27 匹配；共享 README 与 TESTING 的并行 GDAL 文档修改已逐项确认，不纳入 DuckDB 行为验证。A 另验证签名拒绝、安装缓存不一致、官方兼容模式，以及 GeoJSON/GPKG/Shapefile 空与 NULL 几何。B2 验证 140,000 行大整数及 nullable Unicode 批次保留、精确图层名、工程与垂直 CRS、五个晚失败切点和同 Session 恢复；已消费前缀后的 GDAL 故障仍未验证。

主 agent 在 R2 完整 feature 重跑中复现两项 HTTP fixture 失败。实际 accepted socket 标志与传输诊断证明 macOS 继承 O_NONBLOCK，读 header 立即返回 WouldBlock。显式恢复 blocking 后，R3 两位独立复现并反证旧行为；隔离与共享工作区的新构件全套检查均通过。

R3 的 SQL 空间过滤结果正确，但尚未证明命中了 GDAL 新增的 NULL 过滤分支。B2 的 8205 行 HTTP 探针在首组两次恢复成功后被主动中止，随后 64 行探针完成六次失败/恢复；被中止运行不计完整通过，也不是性能基准。

R4 确认固定 Spatial 不再注册旧文档表中的 `spatial_filter_box` / `spatial_filter` 参数，实际调用均被拒绝；没有为追求覆盖而恢复这些接口。原生 NULL 过滤和 geometry-ignored 分支仍未动态验证。B2 在四种格式上各执行三轮消费后取消、consumer error、panic，迟到 token 未影响后续读取，保留 Arrow 值和 metadata 在关闭后仍正确。

R5 两位完成时各 29/29 文件和新构件哈希匹配，B2 已达到个人四个完整轮次；空名问题尚未被这些结果关闭，因此继续第六轮。B2 初次 CSV EMPTY oracle 使用普通 OGR WKB 导出时得到 POINT(0 0)，经源几何的 WKT/IsEmpty 与 ISO WKB 对照确认是 oracle 问题；采用 ISO WKB 后，双几何、NULL/EMPTY、原 WKT、字段投影与往返通过。该初次失败不计产品缺陷，也未算通过。

R6 使用源及安装构件 SHA-256 `41f44ad005b78fa2cfc4dd8ce26bbe254ee7bb7ff99e1db495e6fab890d2d2ce`，直接复测原始失败文件并核对原文件哈希未变。空名无冲突时保持 C2；前后不同位置的大小写/保留后缀冲突均保留所有原值、几何位置及 CRS。审查后只更新本文件与验证记录的结论，运行实现、测试、配置和原生构件未再改变。

A 独立验证 10,000 行 Unicode 长字符串与大 Int64 批次，在重复扫描和 Session 关闭后仍完整；带显式 FeatureCount/Extent 的 VRT 在 schema 成功后，于五个有效行数切点返回 `fetch Arrow batch` 错误，导出均未发布。消费者回调在这些故障用例中仍收到零行，未据此声称验证了已消费前缀后的 GDAL 失败。

B 独立复测损坏 FGB、GPKG、Shapefile、错误公钥拒绝、8 个 Session 共用稳定扩展路径并恢复读取，均符合预期。R1 正式报告已落盘且哈希复核完成；B 的最终消息发送被工具系统中断，不将该消息视为额外一次审查。后续轮次分别计数。

以下是**原生修复前**两位审查者各四轮、共八份报告的历史记录。当时七项库边界问题已修复，另两项 GDAL 问题通过禁用 reader 阻断；当前修补版本已直接处理这两项问题，见 [新验证](DUCKDB_VALIDATION.md)。

## 旧四轮记录

| 轮次 | 快照 SHA-256 | 审查 A | 审查 B | 处理 |
| --- | --- | --- | --- | --- |
| 1 | `4170efb60b6c50cd22d090af532b1768ce896154e7c6718989bc5e5925bb2b7b` | 两个 P1：嵌套几何、冲突 CRS 注解 | 一个 P1：两条凭据重定向路径 | 已修复，后续轮次交叉复核通过 |
| 2 | `2c034af4093c7905b997101f1feb628868466d3a4759eb253d6c4b5813e7da7c` | P1：GDAL 错误被吞；P2：打开选项与 metadata 不一致 | P1：HTTP/S3 scope authority 边界；交叉验证首轮修复 | scope 已修复；GDAL reader 失败关闭，原生修复仍未完成 |
| 3 | `9a9f22b7b75681ac762840876bce228e4210673f8eae01ab22379f94d3c11558` | 两个 P1：INTERVAL 微秒、TIMETZ 偏移丢失 | P2：取消注册遗留 view；交叉复核 scope/失败关闭 | 类型递归拒绝、注册事务回滚；主 agent 同步修正失效示例 |
| 4 | `8274c81c6c4a973c48b9f22882fee819640830591c49e924b4d833dce5a595f8` | 无新增确认阻塞项；类型、取消、并发发布复核通过 | 无新增确认阻塞项；1500 次取消竞态、旧关系与慢 HTTP 复核通过 | 当前启用范围收束；保留 GDAL 阻断及未验收项 |

## 第一轮确认问题

1. **嵌套几何逃逸 GeoParquet 输出契约（A，P1）**。根级几何之外的 `List<GEOMETRY>`/`Struct` 字段会保留原生 Parquet 几何注解，M 坐标也不会经过根级检查。现递归拒绝嵌套几何，保留普通嵌套属性；以 List M、Struct XY 和普通 Struct/List 属性回归。
2. **原生 Parquet 与 GeoParquet 的 CRS 相互冲突（A，P1）**。物理字段 `GEOMETRY('EPSG:3857')` 与省略 CRS 的 `geo` metadata（含义为 CRS84）同时存在时，DuckDB 选择前者。现检查 `parquet_schema.logical_type`，对原生 GeometryType/GeographyType 注解明确拒绝；不使用可能已被 `geo` metadata 转换的 `duckdb_type` 判断。
3. **跨源重定向转发凭据（B，P1）**。固定版本 httpfs 在 302 后移除 Authorization，但转发自定义 HTTP header 和 S3 `x-amz-security-token`。公开设置不能关闭跟随重定向。现创建 secret 前拒绝自定义 header、多重 Authorization 和所有 `Some(session_token)`；支持单一规范化 Authorization、静态 S3 密钥及调用方生成的预签名 HTTPS URL。本机测试覆盖跨主机/跨端口、GeoParquet/GeoJSON/FlatGeobuf、HTTP/S3 两条路径。

原生重定向证据：[httpfs curl](https://github.com/duckdb/duckdb-httpfs/blob/77392a72077a85897db2519fae11e2c0da972b8a/src/http/httpfs_curl_client.cpp#L263)、[httpfs httplib](https://github.com/duckdb/duckdb-httpfs/blob/77392a72077a85897db2519fae11e2c0da972b8a/src/http/httpfs_httplib_client.cpp#L97)、[公开设置](https://github.com/duckdb/duckdb-httpfs/blob/77392a72077a85897db2519fae11e2c0da972b8a/src/http/http_settings.cpp#L132)。第一轮未确认的 scope 疑点在第二轮完成验证和修复。上述多格式重定向测试属于第一轮历史记录；GDAL 禁用后的常驻回归使用 GeoParquet。

## 第二轮确认问题

1. **凭据 scope 不限定 authority（B，P1）**。`http://127.0.0.1:5147` 会匹配端口 `51478`；`s3://bucket` 会匹配 `bucket-extra`。修复为验证非空 ASCII authority、拒绝 userinfo/模糊边界，并给 authority-only scope 补 `/`。使用 `which_secret` 核对正负匹配；另用真实本机 HTTP 请求确认错误端口不带 Authorization。S3 的原始证据是错误选用签名凭据，不是明文发送静态 secret。
2. **GDAL 原生失败被吞，成功发布损坏结果（A 发现，B 扩展验证，P1）**。VRT/SQLite 在第二条错误后返回一行成功；FGB 截断、GPKG 页面损坏可返回零行成功；Shapefile 截断仍返回 100 行但 99 个 geometry 变 NULL。`ST_Read_Meta.feature_count` 同样可能截断，重复计数不能补救。当前保留官方签名策略并在任何来源 I/O 前拒绝 `Reader::Gdal`；初始多格式目标仍受此上游缺陷阻断。高级 SQL 仍是可信调用方入口，不应直接调用有缺陷的 `ST_Read`。
3. **打开选项与 metadata 不一致（A，P2）**。`ST_Read_Meta` 不接收 open options；例如 GPKG `LIST_ALL_TABLES=NO` 会造成错误的图层枚举/选择。当前该路径随 GDAL reader 禁用；重新启用前，带选项的读取必须一致探测，或要求显式 layer 并说明未经选项处理的列表不能用于验证。这不是已经修复原生接口的声明。

固定 Spatial 的 [Scan](https://github.com/duckdb/duckdb-spatial/blob/a3dbcd1781ec26c3a1be9bd622be05b376d6f47a/src/spatial/modules/gdal/gdal_module.cpp#L1138) 把 Arrow 非零返回码当 EOF，[CPL handler](https://github.com/duckdb/duckdb-spatial/blob/a3dbcd1781ec26c3a1be9bd622be05b376d6f47a/src/spatial/modules/gdal/gdal_module.cpp#L2277) 忽略普通失败；构件所用 GDAL 3.12.4 的 [Arrow loop](https://github.com/OSGeo/gdal/blob/v3.12.4/ogr/ogrsf_frmts/generic/ogrlayerarrow.cpp#L1999) 还可能返回零码和部分批次。恢复条件：原生调用前重置 CPL error；调用后同时检查返回码和 CPL failure，覆盖非空部分批次；正确释放 Arrow 资源后从安全边界报告错误；通过 FGB/GPKG/Shapefile/VRT 损坏数据与网络中断回归。不得仅用驱动黑名单、行数比对或修改 EOF 分支代替。

补充复现脚本：`/private/tmp/spatial-duckdb-review-b-r2/examples/review_b_corrupt.rs`；损坏样本与 `ogrinfo` 对照：`/private/tmp/spatial-duckdb-review-b-r2/corrupt-fixtures`。这次补充核对不计为第三轮。

## 第三轮确认问题

1. **INTERVAL 微秒丢失（A，P1）**。Parquet 将 `...05.123456` 回读为 `...05.123`；嵌套 `Struct<List<Interval>>` 同样截断，Arrow 类型相同无法发现。现复用字段递归检查，在文件导出前拒绝所有层级 Interval；Arrow 消费仍保留微秒。
2. **TIMETZ 偏移与比较语义丢失（A，P1）**。相同本地时间的 `+05:30` 和 `+00` 在 SQL 中不同，Arrow 却产生相同且无偏移 metadata 的 Time64；Parquet 回读与原值也不等价。现用绑定已有安全 `column_logical_type` API，在参数化零行查询中检查原生类型，递归拒绝 TIMETZ，覆盖 schema、批次及文件输出；没有解析类型字符串或引入 unsafe。
3. **取消注册遗留 view（B，P2）**。此前自动提交 CREATE 后的最终取消检查可返回 `Cancelled`，同名重试失败。现创建 view 使用事务，取消监听 join 和最终检查结束后才提交，失败显式回滚并保留回滚错误。48 次不同取消时序的公开 API 回归核对无遗留、同名重试及旧 view 保护。

主 agent 的完整 feature 检查另外发现示例仍从已禁用的 GDAL reader 读取，已改为在临时目录生成 GeoParquet 再完成读取、Buffer、导出和批次消费。修正后的 25 个 DuckDB 回归全部通过；完整检查以最终验证记录为准。

## 第四轮复核

- A：TIMETZ 八种形态覆盖别名、固定数组、Map key/value、Union、深层嵌套，有行/零行和参数化结果均拒绝且无消费回调；Interval 六种形态保留流式精度、导出拒绝；普通时间类型和显式文本转换可用。96 次注册竞态无取消遗留；两个 Session 同目标发布只有一个成功，内容和清理符合预期。
- B：25 个 DuckDB 测试及示例通过；1500 次注册竞态为 1499 次取消、1 次成功，无遗留 view；41 次不同来源的同名注册保留旧值。慢速本机 HTTP 的单次取消观测为 216ms，随后重试读取 100 行成功；该时间不是延迟保证。
- 主 agent：独立快照与共享工作区的默认/feature 检查全部通过。最终 13 个实现/配置文件均与第四轮冻结指纹一致；此后仅补充文档中的审查结果和验收边界。

第四轮探针分别位于 `/private/tmp/duckdb-review-a-k9g7nmzf/examples/review_a_round4.rs`、`review_a_types4.rs` 和 `/private/tmp/spatial-duckdb-review-b-r4`。执行日志、计数与未运行项目见 [验证记录](DUCKDB_VALIDATION.md)。

## 证据与边界

轮次源码归档及逐文件 SHA-256 保存在 `/private/tmp/spatial-duckdb-review-20261118-0mlbkhjn`。A 的初次探测在 `/private/tmp/duckdb-review-a-k9g7nmzf`；B 在 `/private/tmp/spatial-duckdb-review-b-r1`。临时探测复用或克隆已编译 target，只用于运行时行为验证；独立源码构建证据另见 [验证记录](DUCKDB_VALIDATION.md)。

未执行真实 AWS S3、完整 HTTPS/TLS 与重定向矩阵、Linux/远端 CI、生产规模压力测试、强制提交/回滚失败、磁盘耗尽或清理权限故障。没有提交或推送代码，也没有发布原生修复构件。

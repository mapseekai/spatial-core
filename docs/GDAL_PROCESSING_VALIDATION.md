# GDAL 栅格与矢量处理验证记录

日期：2026-11-14。相对基线 `3b6c099` 的工作区实现；本轮尚未提交或推送。设计见 [GDAL_PROCESSING_DESIGN.md](GDAL_PROCESSING_DESIGN.md)。

以下前三节保留首轮实现及其两位审查者的验证记录；最新六项修复的结果见文末“六项对抗审查修复验证”，不将前一轮通过记录当作本轮证据。

## 首轮环境与自动化

macOS ARM64，Rust 1.98.1，`gdal` 0.19.0 / `gdal-sys` 0.12.0。本机 headers、库、数据和 pkg-config 统一使用 `/opt/homebrew/Cellar/gdal/3.12.2_2`，运行 GDAL 3.12.2；PROJ 网络关闭。未修改用户级 Cargo 配置或相邻项目源码，未新增依赖。

| 检查 | 本轮结果 |
| --- | --- |
| `bash scripts/ci/check.sh` | 通过：结构检查、rustfmt、Clippy all-targets `-D warnings`、Rustdoc `-D warnings`、diff 空白检查 |
| 默认库测试 | 93 通过，0 失败，26 忽略；忽略项在下面独立执行 |
| doctest | 4 通过 |
| 自包含示例 | `raster_convert`、`raster_process`、`vector_convert`、`vector_process` 均通过 |
| 可选 GeoParquet roundtrip | 显式 `--ignored` 执行，1 通过 |
| `bash scripts/ci/test-postgis.sh` | 隔离库中全部 25 项通过，0 忽略 |

PostGIS 使用本机已有 `imresamu/postgis:17-3.6-alpine3.22` 镜像，新建可丢弃容器，仅绑定回环地址；PostgreSQL 17.7、PostGIS 3.6.1、数据库端 GDAL 3.10.3。合成 OutDB fixture 目录以相同绝对路径只读挂载。未连接业务库；测试后容器已停止并自动删除。25 项回归覆盖原有导入、重投影、像元读回、事务与取消路径，不能视为本轮桌面数据再次入库验证。

运行日志：`/private/tmp/spatial-core-gdal-processing-ci-final.log`、`/private/tmp/spatial-core-gdal-processing-postgis.log`。默认测试包含故意触发的 GDAL 错误、取消及非法路径日志；以测试断言及退出码判断结果。

## 桌面真实数据

独立临时 Rust 消费者从当前源码构建，调用公开 API；输出写入新临时目录，关闭后重新打开断言。真实输入来自 `/Users/zhang/Desktop/测试数据`，未修改源文件。栅格转换前后核对源文件长度及修改时间；这不是源文件全量哈希校验。

| 输入 | 执行与断言 |
| --- | --- |
| `高景.tif`，15580×9518，3×UInt16，NoData 65535 | 两个相邻 512×512 窗口转换 GTiff；逐波段核对全部像元、类型、NoData、CRS、仿射偏移 |
| `GJ_COG_zip.tif`，同尺寸、3×UInt16 | 512×512 窗口输出 COG；相同像元/元数据断言及 `LAYOUT=COG` |
| `JL1KF01B_PMSR5_20230517113518_昆明世纪城.tif`，11458×11458，3×Byte，投影 CRS | 512×512 窗口输出 COG；像元、CRS、像元尺寸及窗口位置精确核对 |
| GF2 `...-MSS1.tiff`，7300×7892，4×UInt16，无 CRS | 512×512 转换 GTiff，随后 FillNodata `Mask::None`；1,048,576 个样本不变，无 CRS 状态保留 |
| SVN3 `...-Cloud.tiff`，14129×14586，Byte | 选择起点 (8685,2706) 的双类别窗口；Polygonize 得到 2 个面，再 Rasterize，262,144 个类别像元完全一致；Sieve 阈值 8 读回，未产生源类别集合之外的值 |
| 两个高景窗口 | BuildVrt 生成 1024×512×3 拼接，逐样本与原图对应窗口比对；VRT 及源窗口继续保留供读回 |
| 高景窗口 | Warp 到 EPSG:3857，512×512×3；核对 CRS、尺寸、正负像元间距，第一波段 227,092 个非 NoData 像元 |
| `全球国家边界.fgb`，EPSG:3857 | 以源单位 1000 米简化，保留 177 个要素、168 个属性字段；另重投影到 EPSG:4326 后裁剪至 [90,0,140,55]，得到 19 个要素，所有几何包络在范围内 |
| `测试的shp数据3857.shp` | 栅格化到 512×512，读回仅含 0/7 且含有效烧入像元，输出 EPSG:3857 |
| `line2 2.geojson` | 按源范围计算加密距离，输出保留 385 个要素 |
| `point.geojson` | SQLite SQL buffer，8 个输出面均有正面积 |
| GF2 `...-PAN1.tiff` | 原生读取即报告 TIFF 目录损坏；库返回 `Open / NotModified`，没有创建输出；未尝试修改源数据 |

五个正常栅格源共抽取六个窗口，各窗口的转换像元和元数据全部核对。第一次真实矢量探针把国家边界的投影坐标误作经纬度，得到空裁剪；核对源 CRS 后改为重投影后的目标裁剪，最终全部断言通过。这是探针坐标系假设修正，没有因此修改库的裁剪语义。

探针及最终日志保留于 `/private/tmp/spatial-core-processing-real-5k2r6d94/src/main.rs` 与同目录 `verified.log`；输出在 `outputs-verified/`。该外部消费者的离线依赖解析独立于仓库锁文件，仓库 CI 和数据库回归仍使用仓库锁定依赖。

真实 Cloud 窗口的 Sieve 没有改变像元，不能据此证明小区域消除；默认微型 fixture 实际验证了孤立类别被替换。填洞有洞插值、地形坡度/颜色表、三维等值线等由有明确预期结果的合成 GDAL 文件验证；本次没有真实 DEM。

## 两位 Astra max 复审

使用两个独立 `gpt-6-astra`、`reasoning_effort=max` 子 agent，分别审查 API/参数/算法语义和 FFI/资源/数据安全。确认的 10 类问题均已修复，仓库增加对应回归；两位审查者针对当前源码重新构建外部消费者并复验，均无剩余已确认问题，结论为可交付。

| 问题 | 修订与回归 |
| --- | --- |
| 具体几何类型静默丢失 Z/M | 必须显式 Dimensions；`-nlt` 生成 Z/M/ZM 后缀；测缺维度拒绝、三种维度实际坐标、显式 XY 降维及语义转换保留 |
| Shapefile 字段缩名导致 Polygonize/Contour 失败 | 按新增字段位置调用算法；读回分类/高程值，保留原生缩名警告 |
| Sieve 遗漏 Int8 | 支持有符号 8 位；实际移除中心负类别像元 |
| Contour 错把多波段联合 NoData 当单波段掩膜 | 默认拒绝不支持的 mask flags；验证显式忽略和 NoData 路径 |
| 非 Byte 显式 mask 静默改变有效性 | typed `Mask::Band` 要求 UInt8，三个算法均验证创建前拒绝和合法 mask 面积 |
| COG/FillNodata 覆盖或删除临时文件、源文件 | 独占 scratch 子目录；验证邻接文件、定制 CPL_TMPDIR、取消/panic、配置恢复和清理 |
| 小写原生驱动名漏过 world file 保护 | 使用 GDAL 标准驱动名检查；GTiff/gtiff/gTiFf 均拒绝覆盖源伴随文件 |
| 辅助颜色表/裁剪源被输出覆盖 | 颜色表及可识别本地 cutline 文件集合参与别名预检；冲突时 NotModified |
| 混合像元类型复制截断未处理波段 | Fill/Sieve 创建前拒绝混合类型 VRT；源 Float64 值保持不变 |
| FillNodata 改写有效 Float64 精度 | 全有效输入跳过插值；实际插值拒绝不能由 Float32 精确承载的像元类型 |

API 审查另执行了 21 个具体几何类型缺维度组合，以及三种语义转换 × XYZ/XYM/XYZM 共 9 组坐标保留探针。FFI 审查另外验证独立波段 mask、不同 Int64 NoData 复制，以及 COG/Fill/Sieve 的 6 个取消/panic 场景；Fill/Sieve 在复制完成后才中断原生算法。两位 agent 没有编辑共享源码。

GitNexus 索引落后基线两个提交，CodeGraph 也不是当前完整工作区快照；只作为定位线索，最终判断基于当前源码、原生 API 和实际消费者探针，未把图结果算作完整影响验证。

当前工具环境未提供 Multica 连接，未同步外部 issue；确认问题、复现及修订保留在本记录和对应回归测试中。

## 未验证及限制

- 未执行 Windows、其他客户端 GDAL 版本或远端 CI；数据库端 GDAL 版本的测试不等于客户端版本兼容验证。
- 大型栅格仅处理指定窗口，未执行整幅处理、内存峰值、长时间负载、真实 DEM 科学精度或磁盘耗尽注入。
- Warp 实测核对了元数据和有效像元，未逐像元建立独立重投影数学基准；矢量属性核对以字段数量为主，不宣称跨格式属性完全无损。
- 原生输出覆盖仍由调用方显式承担；任意 VSI、数据库、插件和其他原生参数隐含辅助资源不具有通用副作用隔离。类型化目标目录仍要求调用方独占。


## 六项对抗审查修复验证（2026-11-14）

三方对抗审查确认的六项意见已逐项处理。保留现有 Builder 与统一执行结构，没有新增库依赖或扩大公共 API；验证完成时尚未提交或推送，后续交付状态以 Git 历史为准。

验证针对本轮工作区源码。复审期间分支合入了已有的跨 GDAL 版本进度回归，当前 HEAD 为 `4ed0c8038c25ee09a13f8ddb5777c8b0079f50a6`；完整检查日志已包含该回归。初次测试的 73 个源码、示例及构建文件 SHA-256 与复审开始时逐一一致；复审新增的关闭修复只改变 `gdal_ext/execution.rs` 与 `raster/execute.rs`，随后重新执行全部测试。临时目录中的 `source-six-fixes-manifest.json` 和 `source-manifest.json` 分别记录修复前、最终源码。GitNexus 仍停留在 `3b6c099`，CodeGraph 也不是当前完整快照；审查以当前源码和实测为准，未把过期图索引算作影响验证。

| 意见 | 修复与新增回归 |
| --- | --- |
| BuildVrt 丢失源打开选项 | 在共享 utility 写入前拒绝带源打开选项的 typed/native BuildVrt；返回 Unsupported / Prepare / NotModified；overview fixture 验证原输出不变，以及 Translate VRT 代理再拼接后关闭读回的准确像元 |
| 重复别名预检 | 单次执行内复用输入/输出文件身份；保留驱动与扩展名目标并集和所有已知源，新增驱动目标仍检查已有源；测后补目标、同路径、目录包含、软/硬链接、伴随文件 |
| 多源准备期取消迟缓 | 每个来源打开前检查 Control；确定性迭代器测试在首个来源打开后取消，第二个不存在的来源不被打开；已有来源仍关闭 |
| 波段错误契约不一致 | 七类 band 入口和三类 Mask::Band 共用 source_band；覆盖零、实际越界、usize::MAX 共 30 个组合，核对类别、阶段、NotModified 和输出不存在 |
| 空 Control 仍注册回调 | optional ProgressCallbackState 只在有回调/token 时安装函数和 userdata；测试空控制、仅 token 和已有并发/panic/取消桥接 |
| 未使用 Dataset 所有权分支 | 删除 Dataset variant、ManuallyDrop、手工 Drop 与 relinquish；保留路径转换和上游归属；新增低层 Translate/Warp 关闭输出后输入仍可读的回归 |

最终完整 `scripts/ci/check.sh` 通过：**102 项默认测试、4 项 doctest、格式、Clippy all-targets -D warnings、Rustdoc -D warnings、结构检查、四个示例、diff 检查**。26 个默认忽略项另行执行：可选 Parquet **1 项通过**；专用 PostgreSQL 17.7 / PostGIS 3.6.1 / 服务端 GDAL 3.10.3 **25 项通过，0 忽略**。初次六项修复为 101 项默认测试，复审追加关闭回归后全部重跑。未改用户级 Cargo 配置，仓库检查使用锁定依赖。最初沙箱内的非 UTF-8 路径测试被系统权限拒绝，完整检查在正常本地文件系统权限下重新执行并通过，未以忽略该测试替代验证。

本轮临时消费者在新 target 目录从当前源码独立编译，重跑上表桌面数据处理的全部断言：六个栅格窗口的像元/类型/CRS/NoData、COG、1024×512×3 VRT 拼接、Warp、分类面与栅格互转、筛选/复制，以及真实矢量简化、裁剪、加密、SQL buffer。损坏 PAN TIFF 仍安全返回 Open / NotModified。额外以真实 `GJ_COG_zip.tif` 的 overview 测试两个 BuildVrt 入口在写入前拒绝打开选项；使用 Translate VRT 代理后，关闭重开比对 **49,152 个 UInt16 样本全部一致**。原始真实源保持只读，输出均在新的临时目录。

真实 PostGIS 导入另使用本次可丢弃容器，写入后独立 SQL 查询：`point.geojson` 8 个点、`测试的shp数据3857.shp` 385 个要素、`全球国家边界.fgb` 177 个要素，共 **570 个要素**。国家边界的 Polygon/MultiPolygon 混合类型先检查全部几何为二维，再显式转换为 MultiPolygon GPKG 后导入；没有修改原 FGB。矢量计数验证不等于逐字段/逐坐标无损验证。

三组真实 512×512 窗口（高景 3×UInt16、吉林 3×Byte、Cloud 单波段 Byte）各导入 16 个 128×128 瓦片，共 **48 个 InDB 瓦片**。数据库 `ST_Union` 后 `ST_AsGDALRaster` 导出，重新用 GDAL 比较尺寸、波段数、类型、SRID，以及全部 **1,835,008 个样本**，全部一致。OutDB 本轮由前述合成数据库回归覆盖，不宣称真实源 OutDB 已测。导入结束删除随机测试 schema；验证结束停止并移除本次专用容器。

额外尝试的 `line2 2.geojson` 同时含 379 个 LineString 与 6 个 MultiLineString，直接入库按已有 schema 契约被拒绝；显式归一后，因源没有声明 CRS、坐标范围却约为 X 8176528–15025548 / Y 2056665–7082005，按 GeoJSON 默认地理坐标重投影被 PROJ 拒绝。没有推断或覆盖其真实 CRS，也没有把这份数据计为成功入库；前述纯加密处理不涉及重投影，仍已通过。失败尝试和最终成功日志均保留。

本次全部日志及临时消费者位于 `/private/tmp/spatial-core-six-fixes-20261114/`：`ci.log`、`parquet.log`、`postgis.log`、`real.log`、`real-vrt.log`、`real-postgis-normalized.log`。外部消费者锁文件独立解析缓存中的兼容依赖，不把它的依赖版本等同于仓库锁文件。

复审关闭修复后重建专用测试容器，并在新的 `real-close/outputs` 目录重跑上述全部真实处理与导入断言，全部通过。最终日志为 `ci-close-final.log`、`parquet-close-final.log`、`postgis-close-final.log`、`real-close-final.log`、`real-vrt-close-final.log`、`real-postgis-close-final.log`。真实消费者重编当前库源码，复用初次独立构建的依赖缓存；不将复跑算为冷构建。最终查询确认真实导入的临时 schema 数为零。

### 性能与取消复测

release 构建，原生与封装交替执行 5 次，关闭后读回；计时包含相同输入打开、输出创建、算法及关闭。

- 单 VRT 引用 5,000 个文件的 1×1 窗口转换：本轮原生中位数 **32.884 ms**、封装 **149.373 ms**；上一轮封装为 **4.908 s**。剩余耗时包含必要的文件集合保护，未承诺与裸 GDAL 相同，也未将跨轮机器负载差异当作精确加速比。缓存随输入文件数占用额外内存，仅在本次执行中存在。
- 默认空 Control 的 Polygonize：第一轮本机计时波动较大（1×500000 原生/封装 62.463/81.402 ms），因此原样保留日志并复跑。第二轮中位数为 1×500000 **61.601/60.495 ms**，8192² **434.724/434.019 ms**；原先细长输入百万次 Rust 回调开销已移除。其他形状的浮动不解读为封装比原生更快。
- 128 来源：约 1.361 ms 取消，1.417 ms 返回，峰值 15 FD；512 来源：约 0.948 ms 取消，1.019 ms 返回，峰值 14 FD。基线均 4 FD；取消后未继续打开全部输入，无输出文件。

复测日志：`performance-alias.log`、`performance-progress.log`、`performance-progress-repeat.log`、`performance-cancel.log`；无机器相关的硬性 CI 耗时阈值。

追加关闭修复后重新编译 release 消费者，并在其他验证结束后顺序重测，日志为 `performance-{alias,progress,cancel}-close-final.log`。5,000 文件中位数原生/封装为 **24.703/82.176 ms**；空 Control Polygonize 的 1×500000 为 **40.082/40.508 ms**、8192² 为 **420.071/418.398 ms**。128/512 来源分别在 6.005/0.707 ms 取消、6.303/0.772 ms 返回，峰值 15/14 FD，基线 4 FD。保留前述较慢轮次供比较，不将缓存/机器负载差异归因为最后几行关闭修复带来的加速。

### 复审与边界

两位独立 `gpt-6-astra`、`reasoning_effort=max` 子 agent 完成静态检查、独立消费者探针和双向交叉质疑。复审发现并共同确认一项新增 **P2：VRT 关闭写盘失败却报告成功**。typed BuildVrt 输出到不可写父目录、native BuildVrt 输出到已存在空目录时，GDALClose 可能返回 CE_None，但 CPL 报 Failure 且没有产物。共享 `execution::close` 现复用 `checked_gdal_call`，只检查本次关闭的返回值与 CPL 错误，再由原有 `record_close` 保留主错误并追加独立清理错误。真实默认/仅 token 回归已验证修前失败、修后通过；日志为 `close-regression-before.log` 与 `close-regression-after.log`。

| 复审 | 独立反例与最终结论 |
| --- | --- |
| A：契约、API、资源 | 两个来源使用不同 overview，分别代理后 typed/native 拼接与四波段重排逐像元一致；源选项所有位置拒绝、30 个 band/mask 边界及后续来源不足；空 Control 惰性读取错误；typed/native 真实关闭失败与低层借用源寿命。关闭修后全部复验，无剩余已确认问题 |
| B：性能、预检、复用 | 12 个公开 API 场景覆盖 VRT 内部依赖、辅助颜色表/cutline DBF 硬链接、后补驱动与扩展名并集、目录边界、矢量伴随文件、token-only 取消、重复失败 FD；独立重现关闭问题；`-skipfailures` 保留执行期诊断且不产生虚假关闭错误。关闭修后全部复验，无剩余已确认问题 |

两位最终均给出可交付结论。A 的日志及源码在 `/private/tmp/spatial-core-six-fixes-contract-review-a/`（`close-before.log`、`close-after.log`）；B 在 `/private/tmp/spatial-core-review-b-20261114/`（`close.log`、`close-fixed.log`、`regression-fixed.log`）。B 的 `-skipfailures` 初次探针误将“保留 NULL 几何的失败行”期待为删除行；核对原生契约后改为独立 SQL 验证 2 行、1 个非 NULL 几何，未修改库以迁就探针。复审者未代替主代理重跑完整 CI/数据库；主代理在最后修复后执行的完整结果如上。

尚未验证其他平台/客户端 GDAL、远端存储、生产并发、真实磁盘耗尽和整幅大影像内存峰值；前述窗口读回不等于完整影像处理。BuildVrt 源打开选项目前采用显式拒绝与代理 VRT 路径，尚未直接支持逐来源选项持久化。当前无 Multica 连接，修订及证据保存在仓库文档与回归中。

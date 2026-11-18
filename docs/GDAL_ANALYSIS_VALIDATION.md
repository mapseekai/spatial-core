# GDAL 查询与分析验证记录

日期：2026-11-18。对应 [实现设计](GDAL_ANALYSIS_DESIGN.md)。除单独链接的 GitHub Actions 记录外，以下结果均来自本地；未作发布或生产验证。

## 环境与范围

- macOS arm64，Rust 1.98.1，gdal 0.19 / gdal-sys 0.12。
- 实际 GDAL 3.12.2，链接的 GEOS 3.14.1。CleanCoverage 正向路径在本机执行。
- Overlay 的 C++17 输出适配器使用锁文件中已有的 cc 和 pkg-config 作为构建依赖；DuckDB 是同一工作区内独立开发的可选功能。
- 本机 GDAL 未包含 muparser。Builtin 算法正向通过；MuParser 返回 Unsupported，确认最终输出未创建，不把此结果计为公式引擎正向通过。

## 六项修复后的重新验证（2026-11-18）

下面是本次重新执行的结果；修复前如何触发六个问题见[审查快照](GDAL_ANALYSIS_REVIEW.md)。完整检查命令、专用数据库配置和各回归入口见[测试指南](TESTING.md)。

| 问题 | 固定在仓库中的回归与读回标准 |
| --- | --- |
| F1 多来源 VRT 统计 | 拼接和重叠分别对照逐像元结果；保留原有派生像元函数、打开选项及原生 NoData/mask 语义。检查 Exact、Approximate 和直方图，确认读操作没有写回源文件或 PAM。 |
| F2 金字塔刷新 | 奇数长宽、重复刷新、显式和默认选择、内部和外置概览均以物理层数、尺寸和像元为准。局部刷新另查范围外像元；概览指向其他文件时不写入。独立物理层矩阵覆盖 192 组。另有固定回归把 alpha 与调色板局部刷新逐像元对照 GDAL 的整层结果。 |
| F3 叠加字段 | 单侧空选、双侧空选、默认和显式字段均重开输出核对字段名、来源值和 NULL；不允许驱动悄悄增列。 |
| F4 叠加几何 | 除声明类型外，逐要素读回实际 WKB；Polygon 提升为 MultiPolygon，低维相交和混合维度按输出契约处理。39 组对照覆盖不同几何和算子。 |
| F5 重分类波段 | 有效选中波段旁边含 UInt64、复数或独立 mask 时仍能分类并读回；选中不支持波段或越界仍失败。Calculate 继续验证每个绑定数据集的所有波段。两条入口都在写入前拒绝联合 NoData 元组，并以逐波段标量 NoData 为正对照。 |
| F6 覆盖查询取消 | 以 3、256、257 条有效/无效要素分别触发 token + Continue、直接 Break 与结束回调。取消后不返回成功摘要，前置执行错误不被结束检查掩盖。 |

- 本机默认门禁通过：147 项库测试、4 项 doctest、7 个示例，包含结构、格式、Clippy 和 rustdoc。默认跳过的 26 项单独核算。
- 在隔离的 PostgreSQL 17 / PostGIS 3.6.1（含 raster）实例上另跑 25 项，全部通过，包括库内/库外像元读回和回滚。本机 GDAL 包含 Parquet 驱动，另行执行 GeoParquet 往返测试，1 项通过。这样，默认库测试的 26 项忽略项均实际执行过，173 项不同的库测试全部通过（147 + 25 + 1）。
- 重新构建示例后，逐份重跑下表的 9 份桌面数据。9 组均得到期望结果；21 个原件及伴随文件前后 SHA-256 一致。含 5 个无效几何的 Shapefile 只验证了检查和提前停止；三个大栅格是整图元数据加 256×256 窗口处理。
- 另用两景真实 Landsat 像元构造 VRT：不重叠 8,192 个像元、重叠 6,144 个像元。两者精确与近似统计、256 桶直方图均与逐像元对照一致，输入和伴随文件哈希未改变。
- 另用真实 `logo.png`（288×288、RGBA，含 22 种透明度）补测金字塔：内置/外置 × Average/Bilinear 共四组。每组在倍率 5 的目标层检查窗口内 5,928 个波段样值与独立 GDAL 整层结果一致，窗口外 7,528 个、未选倍率 3 的 36,864 个波段样值与刷新前一致；原始 PNG 及 SVG 的哈希不变。验证使用临时 GTiff，未写入原图。

DuckDB 可选门禁单独计数。提交前使用 `fa271b3` 对应的[签名 Spatial 构件](DUCKDB_VALIDATION.md#原生修复后的验证)（SHA-256 `41f44ad005b78fa2cfc4dd8ce26bbe254ee7bb7ff99e1db495e6fab890d2d2ce`），按[仓库 CI 的编译配置](TESTING.md)重跑 `scripts/ci/test-duckdb.sh`：154 项常规测试、4 项 doctest、25/25 项原生集成，以及 Clippy、rustdoc 和示例全部通过。原生回归包含跨批次读写 FlatGeobuf 的 NULL 和空几何，以及重名或空名字段。此前中间构件的 23 项集成测试不再重复计数。

DuckDB 仍有一个独立的本机风险：保持上述签名扩展和公钥不变，使用 Cargo 默认 Debug（带调试信息）单独运行 `cargo test --locked --features duckdb --lib duckdb::query::tests::spatial_sql_defines_boundary_clip_dissolve_buffer_and_transform_semantics -- --ignored --exact --test-threads=1`，DuckDB 原生分配器的 `allocation_count >= size` 断言会使进程以 SIGABRT 退出。同一用例在仓库 CI 配置下通过；尚未确定根因。

## 四轮对抗复审

两名 GPT-6 Astra Max 审查者各自完成四轮。第一轮独立找反例，第二轮交换测试重点，第三轮交叉复现并区分封装与 GDAL 的行为，第四轮在同一份最终源码上重新调用公共 API、打开产物读回。最后一轮前后，十个相关实现文件的组合 SHA-256 均为 `b58fc3ffe0f1c9444274a905634f2d29abbc0f60fc0a2789e567c3210cd0786f`；这是本次审查范围的指纹，不是 Git 提交号。

| 轮次 | 审查者 A | 审查者 B | 处理结果 |
| --- | --- | --- | --- |
| 1 | 按两轴尺寸核验奇数栅格的物理概览，核对内置和外置存储 | 经公共 API 检查空字段、NULL、中文和不同输出驱动 | 确认原问题的触发方式与输出读回标准 |
| 2 | 重开叠加结果读取二进制、NULL、日期时间、WKB 类型和面积 | 用独立 GDAL 整层结果对照局部刷新，发现 alpha 和调色板的部分像元不同 | 修复有效性掩膜传递以及调色板重采样的网格语义 |
| 3 | 区分封装丢失 alpha 和原生局部调色板与整层结果不一致；交叉核验派生 VRT 统计 | 交叉检查矢量输出与取消；用四像元复现联合 NoData 把第一波段的有效零值丢掉 | 栅格代数在创建输出前拒绝联合元组，增加逐波段标量正对照 |
| 4 | 在 GTiff/VRT 上复验联合与标量 NoData；十组局部刷新检查窗口内外、未选层与原件 | 72 组组合覆盖 alpha、两种色表、六种重采样、两种存储和全图/范围刷新，再查 VRT 和选中波段 | 两人均未发现新的可复现问题 |

第四轮使用独立生成的整层金字塔作为窗口内对照，以刷新前数据作为窗口外及未选层的对照。没有把“命令成功退出”当作像元、字段或几何正确的证据。常设回归命令见[测试指南](TESTING.md)。

## 首次实现的默认检查（六项修复前）

```sh
CARGO_TARGET_DIR=target/gdal-analysis bash scripts/ci/check.sh
```

执行结果：结构检查、rustfmt、Clippy（所有默认 feature targets，warnings 为错误）、默认单元测试、doctest、rustdoc 和七个 GDAL 示例通过。

- 库测试：127 passed、0 failed、26 ignored。
- 文档测试：4 passed。
- 示例：raster_convert、vector_convert、raster_process、vector_process、inspect、raster_analysis、vector_analysis，均执行并读回输出。
- ignored 是 25 项专用 PostGIS 数据库测试及 1 项可选 GDAL Parquet driver 测试，未计为通过。
- 原有非 UTF-8 文件名回归在 macOS 沙箱内出现 EPERM；完整检查在沙箱外通过，没有删除或放宽该回归。

## 首次实现的回归覆盖

| 范围 | 实际核验 |
| --- | --- |
| 元数据与统计 | 缓存/计算结果、直方图计数；主文件及 PAM/mask 字节不变；PAM 覆盖地理信息；已有派生 VRT 函数保留 |
| 精度与像元 | 旋转仿射逆变换；UInt64 原值及 NoData 9,007,199,254,740,993 精确保留；越界拒绝；独立 mask 有效性 |
| 金字塔 | GTiff 内外置、VRT 外置创建/清除/全刷新；区域刷新保留范围外像元；COG 内改拒绝；已有旁路链接保护；拒绝 OVERVIEW_LEVEL；取消报告 MayHaveChanged |
| 计算 | 多输入、多表达式；独立 Source 打开选项；半像素错位拒绝；NoData 传播/原值运算；Builtin 与缺失 muparser；回调取消/panic 后关闭与清理 |
| 重分类 | 区间开闭与重叠拒绝；输入 NoData 位于区间内部/端点以及 NaN；独立 nodata 映射；哨兵更换后像元和 mask；Unmatched Error/Keep/Value/NoData；Float32 舍入后的哨兵碰撞 |
| 叠加 | 七种操作面积与字段；Boolean 子类型；大小写字段冲突；过滤空结果；边界接触不产生面；点/线裁剪；同数据集多个图层与显式选择；取消 |
| 几何 | 自交、NULL、EMPTY、显式 Allow 策略；线简单性独立于有效性；有界样本、全量问题输出和原身份 |
| 覆盖 | 相邻有效面、重叠、小缝阈值、筛选子集、样本截断、要素预算、空层、源 CRS 距离；清理后复检；共享边界简化；无效覆盖拒绝简化 |
| 生命周期 | Algorithm 输出增加引用后 Finalize；沿用 checked close、诊断作用域与 panic-safe 回调；CPL_TMPDIR 下快照与取消清理；最终输出字段子类型校验 |

真实数据暴露的 NoData 重分类问题已修复：GDAL 会将 NO_DATA 解析为一个单点区间，普通区间覆盖它时产生冲突；更改派生 VRT 的 NoData 元数据还可能改变该输入映射。现在在规则编译时拆分普通区间，并冻结源 NoData 输入为精确常量/NAN，目标哨兵独立设置。参照 [GDAL reclassifier 实现](https://github.com/OSGeo/gdal/blob/v3.12.2/frmts/vrt/vrtreclassifier.cpp) 核对，并用实际像元回归。

## 桌面真实数据

来源为 `/Users/zhang/Desktop/测试数据`。所有源文件与同名伴随文件在执行前后进行 SHA-256 对比；九组均一致。维护和写出只发生在独占临时目录，示例结束自动清理。

| 数据 | 规模 | 实际执行与结果 |
| --- | --- | --- |
| Landsat LM41230331983186FFF03_B1.TIF | 3944×3524，约 13 MiB | 读取全图元数据；第一波段 256×256 复制窗口上的统计、直方图、内外置概览、Builtin 计算、重分类与像元读回通过 |
| 高景.tif | 15580×9518，约 783 MiB | 同上；包含既有 PAM/概览等伴随文件，原件不变；源 NoData 65535 的分类通过 |
| GJ_COG.tif | 15580×9518，约 1211 MiB | 同上；仅维护临时 GTiff 复制窗口；源 NoData 0 的分类通过 |
| 测试的shp数据3857.shp | 385 个面 | 检查完成，报告 5 个无效几何；没有自动修复或继续叠加/清理，不能计为完整处理通过 |
| 全球国家边界.fgb | 177 个面 | 几何与覆盖有效；自交集、覆盖清理、简化及输出复检通过 |
| 市级行政边界简化.geojson | 385 个面 | 几何有效；检查出 219 个 invalid-edge 要素；清理后覆盖有效，简化后再次复检通过 |
| point.geojson | 8 个点 | 完整 schema、精确计数和范围查询通过 |
| line2.geojson | 385 条线 | 完整 schema、精确计数和范围查询通过 |
| building.gpkg | 251014 个要素 | 图层 schema、精确计数和范围查询通过；没有对整个建筑数据运行覆盖清理 |

219 是 GDAL 返回的问题要素数，既不是相交对数，也不是独立边段数。CheckCoverage 不推测 invalid edge 的具体拓扑原因。

可复现调用：

```sh
cargo run --locked --example raster_analysis -- "/Users/zhang/Desktop/测试数据/高景.tif"
cargo run --locked --example vector_analysis -- "/Users/zhang/Desktop/测试数据/市级行政边界简化.geojson"
cargo run --locked --example inspect -- vector "/Users/zhang/Desktop/测试数据/building.gpkg"
```

本机一次已缓存运行中，三个栅格窗口的示例内部总耗时约 0.02–0.07 秒，177 个国家面的分析约 0.49 秒，385 个市级面的叠加、检查、清理、简化约 13.6 秒。这是单次功能验证，包含前置检查及回读，不是基准测试；不包含 Cargo 编译及源文件哈希计算，也未采集峰值内存。不能外推到整幅大图或百万面的覆盖处理。

## 能力边界与未执行项

- 基础接口保持 GDAL 3.7 兼容目标。上述四轮复审后的 [Ubuntu CI](https://github.com/mapseekai/spatial-core/actions/runs/44316958002) 揭示旧版本分支的 `Result` 缺少类型标注，导致 crate 无法编译。使用 3.7.3 头文件在本机复现了同一错误，修正后所有 target 均通过 Clippy，并增加了旧版构建专用的 `Unsupported / Prepare / NotModified` 回归。3.7.3 的独立运行库未在本机测试；原生 C++ 适配器需要与头文件匹配的运行库。
- 四轮复审与桌面数据实测均在 macOS 上完成。远端 Linux/macOS 的结果按提交查看 [Spatial core 工作流](https://github.com/mapseekai/spatial-core/actions/workflows/ci.yml)，未验证 Windows。DuckDB 签名扩展需要独立门禁。
- muparser 正向计算、所有重采样/合并策略组合、所有驱动、断电/磁盘耗尽、生产级并发与超大覆盖集未验证。
- Int64/UInt64 元数据和 PixelInfo 支持精确值；计算统计/直方图的浮点派生视图可能改变大整数的 NoData 归属，当前显式返回 Unsupported。栅格代数也拒绝宽整数、独立 mask/alpha 及跨波段的联合 NoData 元组，需要调用方明确转换。
- 区域概览刷新只支持非旋转仿射；维护只支持明确的 GTiff/VRT 存储，COG 内改、内部/外部混合以及局部波段集合拒绝。
- 覆盖操作先流式写入 GPKG 快照，配置 CPL_TMPDIR 可选择磁盘；无法保留的字段类型/子类型拒绝。原生 GEOS 仍可能整体加载几何，输入要素/顶点预算不是 RSS 上限。
- 统计是 GDAL 存储值统计，不自动应用 scale/offset；独立 mask 不等价于排除条件。PixelInfo.valid 单独报告原生 mask。
- 新输出默认不覆盖已有文件；任何处理失败仍应检查 OutputState。金字塔维护可能改变已有目标，既没有事务回滚，也没有跨进程锁。

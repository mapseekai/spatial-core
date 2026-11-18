# GDAL 查询与分析扩展：四轮对抗审查

本页保留修复前的审查快照与复现。六项修复的回归与真实数据结果见[分析验证记录](GDAL_ANALYSIS_VALIDATION.md)；后续发现的两个栅格边界及最终判断见[四轮对抗复审](GDAL_ANALYSIS_VALIDATION.md#四轮对抗复审)。

日期：2026-11-18。当时结论：**先修复 5 组 P2；另有 1 组 P3 取消一致性问题。** 现有 Source、Builder、Control、Outcome、Report 和私有 FFI 分层可以保留，不需要再重写整套架构。本轮只审查，没有修改业务代码、回归测试或既有设计文档，没有提交或推送。

## 基线与范围

- 工作区：`/Users/zhang/code/spatial-core`，分支 `main`。
- HEAD：`49c642410326eae115faf47e13abe71b700ae006`；审查对象包含其上的未提交 GDAL 扩展，不能把 HEAD 单独当成被审查实现。
- 冻结并复核 34 个 GDAL 源码、示例、检查入口及文档文件的 SHA-256；四轮结束均未变化。[基线清单](/private/tmp/spatial-gdal-review/baseline.json)、[A 的最终复核](/private/tmp/spatial-gdal-review-a/round4-evidence.json)、[B 的最终复核](/private/tmp/spatial-gdal-review-b/round4-integrity.json)。本报告在复核后新增。
- 保留并排除并行 DuckDB 任务的 Cargo、模块、CI、示例和文档改动；默认质量检查并不代表 DuckDB 可选 feature 已验证。
- 实测环境：macOS arm64、Rust 1.98.1、GDAL 3.12.2、GEOS 3.14.1，gdal 0.19 / gdal-sys 0.12。本机没有 muparser 正向计算能力。
- 使用 rust-skills、ponytail-review 与四轮 Rust 审查流程。CodeGraph / GitNexus 用于导航；索引未覆盖本次全部新增文件，结论以当前源码、官方原生实现和公开 API 实测为依据。

两位子 agent 均为 `gpt-6-astra`、`max`，分别为 `/root/gdal_review_a` 和 `/root/gdal_review_b`。每轮完成后才派发下一轮，共八次正式轮次结果。主 agent 另做了取消与落盘几何的独立复现。

## 四轮记录

| 轮次 | Reviewer A | Reviewer B | 对抗结果 |
| --- | --- | --- | --- |
| 1：独立找问题 | API、数值、统计视图、概览语义 | FFI、输出保护、字段投影、资源边界 | 独立找到多源 VRT、默认概览刷新、空字段选择；概览误写仅列假设 |
| 2：交叉反驳 | 复现空投影；验证 ADD_FIELDS 的条件边界；反证概览误写；挑战取消评级 | 复现统计失败并验证外层视图方向；复现取消数量边界；扩大概览误写反证 | 撤回误写 P1；证明全局 ADD_FIELDS=NO 会让正常属性变 NULL；取消事实一致、P2/P3 暂有分歧 |
| 3：边界与资源实测 | 39 项几何矩阵、192 个独立概览 fixture、选中/未选波段对照 | 真实 GPKG WKB 解析、低维结果独立反例、重复刷新、220 次有限资源探针 | 新增几何契约和选波段问题；发现显式刷新也新增重复层，撤回“仅修 factor 即可”的建议 |
| 4：独立裁决 | 核对 B 的资源结论与落盘边界，复核基线及门禁 | 独立复现 Reclassify，复核基线及门禁，重新判断取消严重性 | 双方同意以下 5 组 P2、1 组 P3；不把同一输出契约的不同症状重复计数 |

分组并不减少修复范围：F2 的误拒绝和重复创建、F4 的 Multi 提升和低维过滤，分别有独立验收点。

## F1 · P2 · 多来源 VRT 无法计算统计和直方图

位置：`src/raster/info.rs:425`，尤其 430–442 行。

一个普通 4×1 VRT 波段有两个 SimpleSource，组合像元为 `[1, 2, 10, 20]`。原生读取正常，Info 的 Exact、Approximate 和 Histogram 却返回 `Processing / Inspect / NotModified`。这影响常规拼接 VRT，而非所有 VRT。

`statistics_view` 保留多个 Source，却将普通组合波段改成 VRTDerivedRasterBand 并添加要求单来源的 `real` 像元函数，从而改变了原读取语义。[GDAL 像元函数文档](https://gdal.org/en/stable/drivers/raster/vrt.html#built-in-pixel-functions)

最小修复方向：保留原组合波段，在外层构造单来源统计视图。不能重新打开没有原 Source 选项的裸路径，也不能用关闭整个 PAM 的方式规避回写。外部对照证明该方向能得到均值 8.25、直方图总数 4，并保留打开选项和 PAM；它尚不是实际修复验收。

必要回归：拼接及重叠来源、已有派生函数、Exact/Approximate/Histogram、NoData、独立打开选项、PAM 地理信息，以及源文件和伴随文件字节不变。

证据：[A Round 1](/private/tmp/spatial-gdal-review-a/round1.log)、[B Round 2](/private/tmp/spatial-gdal-review-b/round2.log)。

## F2 · P2 · RefreshOverviews 误拒绝已有层或新增重复层

位置：`src/raster/overviews.rs:189` 和 `src/raster/overviews.rs:280`。

存在两条已独立复现的失败路径：

1. **默认刷新误拒绝。** 5×10、level 4 的已有概览为 2×3；当前 round(max(width/w, height/h)) 反推出 3，随后自己的尺寸验证失败。5×17、level 4 也失败。显式传 `[4]` 的对照成功。
2. **显式刷新新增重复层。** 5×10、level 3 已有 `[(2,4)]`；显式 Refresh `[3]` 连续执行两次均成功，概览数量却从 1 增至 2、再增至 3，尺寸均为 `(2,4)`。Internal 和 External 都发生。

A 的矩阵采用 8 组尺寸、4 种因子、2 种存储、3 种刷新方式，共 192 个独立 fixture；包含非方形和单像元边，所有 Build 均成功：

| 刷新方式 | 失败 | 成功但增加重复概览 |
| --- | ---: | ---: |
| 默认 | 10 / 64 | 14 / 64 |
| 显式原因子 | 0 / 64 | 2 / 64 |
| 根据宽高整数约束推导 | 0 / 64 | 14 / 64 |

这些是缺陷探针的观察结果，不是 192 项验收通过。第二条路径绕过默认推导，因此只改四舍五入不够。GDAL 的概览匹配策略会选择一维并优先匹配二次幂，BuildOverviews 并不提供本接口承诺的“不新增层级”语义。[原生匹配实现](https://github.com/OSGeo/gdal/blob/v3.12.2/gcore/gdaldefaultoverviews.cpp#L478)

最小完整方向：按已有概览句柄刷新，例如使用 GDALRegenerateOverviewsEx，并正确取得对应存储的可写句柄。用户 levels 的匹配可以采用整数尺寸约束；不能继续用 BuildOverviews 的创建语义代替严格刷新。外置句柄、mask、区域刷新和关闭状态必须在实际修复时验证。

必要回归：内外置、非方形、单像元边、非二次幂、连续刷新；层数、尺寸不变且像元更新正确；区域外像元及 mask 保持预期。不能把“显式 levels”描述为通用规避方法。

证据：[A 矩阵](/private/tmp/spatial-gdal-review-a/round3-overviews.log)、[B 连续刷新](/private/tmp/spatial-gdal-review-b/round3-overview.log)。

## F3 · P2 · Overlay 显式空字段投影被自动补回

位置：`src/vector/overlay.rs:199`，创建 schema 后调用原生操作的路径。

双方输入各有 `secret`、`keep` 属性。Intersection 显式 `.input_fields([]).method_fields([])` 后仍输出双方全部属性；Clip、Update 显式 `.input_fields([])` 也补回属性。Update 的自动前缀还会使方法侧部分字段值变成 NULL。

原生 OGR 将零字段结果层视为需要自动建立 schema；当前只在原生调用前检查 schema。[GDAL set_result_schema 实现](https://github.com/OSGeo/gdal/blob/v3.12.2/ogr/ogrsf_frmts/generic/ogrlayer.cpp#L4512)

最小修复方向：**仅在最终选择字段为空时设置 ADD_FIELDS=NO**。外部原生对照证实，无条件设置会连正常字段映射初始化一起跳过，使非空投影的值全部变 NULL，不能采用这种修复。

必要回归：Intersection 双空、Clip/Update 空输入字段、双方非空、仅一侧为空、Update 方法侧属性值。读回实际 schema 和实际字段值，不能只检查面积。这里确认的是投影契约失效；没有真实敏感数据被发布或泄露的证据，不升为 P1。

证据：[B Round 1](/private/tmp/spatial-gdal-review-b/round1.log)、[A 条件设置对照](/private/tmp/spatial-gdal-review-a/round2.log)。

## F4 · P2 · Overlay 未落实输出几何类型和维度策略

位置：`src/vector/overlay.rs:184` 和 `src/vector/overlay.rs:200`。

39 项矩阵中有 6 项不符合输出声明，均返回 Ok 并附带原生警告：

| 操作与关系 | 声明 | 实际 |
| --- | --- | --- |
| 面面 Update：重叠、分离、接触，3 项 | MultiPolygon | 方法侧含 Polygon |
| 面面仅共边 Clip | MultiPolygon | MultiLineString |
| 线只触面角点 Intersection、Clip，2 项 | MultiLineString | MultiPoint |

其余 33 项符合声明，包括面面共边 Intersection 正确为空。B 独立复现了这些边界，并直接通过 SQLite 解析 GPKG 中的 WKB，确认 Update 实际存储类型为 `[6,3]`；不是 GDAL 读取时的显示转换。Info 和 CheckGeometry 仍能读取、几何检查仍有效，所以不能宣称所有下游都会失败。

本组有两个独立机制：

- Update 方法侧直接写入几何，没有落实同维度的 Multi 提升。
- Clip 不读取 KEEP_LOWER_DIMENSION_GEOMETRIES；Intersection 仅在两侧维度相同时执行对应过滤。统一传入该选项并不能兑现所有已开放组合的低维排除策略。[GDAL 原生叠加实现](https://github.com/OSGeo/gdal/blob/v3.12.2/ogr/ogrsf_frmts/generic/ogrlayer.cpp#L4959)

最小完整方向：在输出边界落实同维度 Multi 提升和不允许的低维结果过滤，再核对实际输出类型。只调用 forceToMulti 不能修复已经是 MultiLineString/MultiPoint 的低维结果；只放宽 schema 也不能保留现有语义。Update 方法侧有效要素不能被误删。

必要回归：七种面面操作 × 重叠/分离/接触，允许的点线组合；逐一断言落盘 schema、实际几何类型、数量、属性和内容，不只检查面积。

证据：[A 39 项矩阵](/private/tmp/spatial-gdal-review-a/round3.log)、[B 边界反例](/private/tmp/spatial-gdal-review-b/round3-edges.log)、[B 落盘 WKB](/private/tmp/spatial-gdal-review-b/round3-update-wkb.json)、[A 独立边界复核](/private/tmp/spatial-gdal-review-a/round4-boundaries.log)。

## F5 · P2 · Reclassify 被未选中的波段阻断

位置：`src/raster/reclassify.rs:134`。

两种双波段 VRT 的 band 1 都是合法 Byte `[1,2]`、all-valid；band 2 分别为 UInt64 或带独立 mask 的 Byte。执行 `.band(1).value(1,10).value(2,20)` 都返回 Unsupported / NotModified，无输出。先用 Translate 选出 band 1 后，同一重分类正确输出 `[10,20]`。

根因是 Reclassify 在选择波段前验证整个 dataset，而后续只有选定波段参加运算。公开单波段选择不应被无关波段阻断。

最小修复方向：只验证选定波段，或验证已选出的单波段 view；Calculate 的全输入检查仍需保留。不要把修复变成允许实际参与运算的 UInt64 或独立 mask。

必要回归：上述两种未选波段不阻断；选择不支持的 band 2 仍拒绝；越界索引错误类别正确；合法结果像元及有效性正确。

证据：[A Round 3](/private/tmp/spatial-gdal-review-a/round3.log)、[B Round 4 独立复现](/private/tmp/spatial-gdal-review-b/round4-reclassify.log)。

## F6 · P3 · CheckCoverage 的提前成功出口缺少结束取消检查

位置：`src/vector/coverage.rs:376`，关联 `src/gdal_ext/query.rs:77`。

在首次 snapshot 进度回调中取消共享 token，再返回 Continue：

| 输入 | 结果 |
| --- | --- |
| 3 / 256 条无效面 | Ok，完整 checked / invalid 摘要 |
| 257 条无效面 | 下一检查点返回 Cancelled |
| 3 / 256 条有效面 | 后续检查返回 Cancelled |
| 回调直接返回 Break | 返回 Cancelled |

无效几何分支提前返回，绕过正常成功路径的结束 pulse；query::invoke 仅检查已经记录的 callback interruption。这是确定的数据依赖差异，不是仅有极短竞争窗口。

严重性经过实际争论：B 最初建议 P2，A 建议 P3；第四轮双方同意 P3。设计文档的“调用前后检查取消”支持修补，但公共接口明确采用合作式取消，错误定义要求执行路径已观察到取消。反例返回完整、无效的摘要，不能表述为部分数据被当成全层有效。

最小改善：在提前成功返回前补结束检查，保留原执行错误、关闭错误及输出状态；不需要新增取消架构。

必要回归：invalid 3/256/257、valid 对照、token+Continue、Break，以及资源关闭和错误状态。

证据：[A Round 2](/private/tmp/spatial-gdal-review-a/round2.log)、[B Round 2](/private/tmp/spatial-gdal-review-b/round2.log)、[主 agent 复现](/private/tmp/spatial-gdal-review-root-cancellation.log)。

## 排除项与整体设计判断

- **撤回 VRT 概览越界写入 P1 假设。** 显式 Overview 引用另一 TIFF、与自身 `.ovr` 混合等样本中，Build/Refresh 报只读写入错误，Clear 仅删除目标旁路；被引用 TIFF 字节保持。不能仅凭身份检查看似不充分就宣称数据破坏，也不能据此证明所有版本和驱动安全。
- **撤回 vector::Info 结束取消遗漏。** 完整源码已有结束 pulse，早先图索引截断造成疑点。
- **未发现可证实的 Algorithm 双重关闭或悬空引用。** 增加输出引用再 Finalize 的路径与当前读回相符；这不是对所有原生失败路径的形式化证明。
- **不列 Unmatched::Keep 的显式类型窄化为新缺陷。** 当前接口已经说明结果受输出转换影响，不能把它改述为无损承诺。
- **不把代码长度、风格偏好和未测复杂度列为问题。** 现有执行器边界、RAII、配置作用域、诊断限额、只读查询结果所有权值得保留。真正需要修补的是原生选项与类型化契约之间的上述缺口；没有证据要求新增通用 Operation trait、执行框架或第二套表达式引擎。
- **复用应服从实际参与的输入。** F5 应复用单波段验证，不应让 Calculate 和 Reclassify 为复用而拥有错误的相同验证范围。

## 本轮执行的验证

在冻结源码上运行项目规定的默认质量门禁，退出码 0：

```sh
rtk proxy env CARGO_NET_OFFLINE=true \
  GDAL_HOME=/opt/homebrew/Cellar/gdal/3.12.2_2 \
  GDAL_CONFIG=/opt/homebrew/Cellar/gdal/3.12.2_2/bin/gdal-config \
  CARGO_TARGET_DIR=target/gdal-analysis \
  bash scripts/ci/check.sh
```

结果：结构检查、rustfmt、Clippy、127 项单测、4 项 doctest、rustdoc 和 7 个示例通过。26 个 ignored 为 25 项 PostGIS 专库测试和 1 项可选 Parquet 测试，均没有执行。既有非 UTF-8 文件名回归需要在 macOS 默认沙箱外运行，没有删除或放宽测试。[完整门禁日志](/private/tmp/spatial-gdal-review/check.log)

有限资源测试为 20 次预热 + 200 次测量调用，覆盖查询/处理成功、部分失败、取消及 panic 路径。迭代末尾 `/dev/fd` 为 5、GDAL 可枚举 dataset 为 0、block cache 为 0；每次检查的 scratch 为空、线程配置恢复；22 次预期 callback panic 被捕获。取消/panic 重点覆盖 SimplifyCoverage。RSS 未取得，这些指标只证明已测串行路径没有相应持续增长，不能推出全部原生分配无泄漏或大规模性能可靠。[资源日志](/private/tmp/spatial-gdal-review-b/round3.log)

最小探针均在 `/private/tmp/spatial-gdal-review-{a,b,root}`，以当前仓库 path 依赖编译，不修改库测试文件。A 的 `round3` 在完成几何和重分类部分后，因发现重复概览而触发断言；随后独立的 `round3_overviews` 完成全部 192 项并输出汇总。不能把前者失败解释成几何矩阵未执行，也不能把后者退出 0 解释成反例全部通过。

## 边界与交付状态

- 本轮未重新执行桌面九组真实数据流程；此前结果仍见 [实现验证记录](GDAL_ANALYSIS_VALIDATION.md)，不冒充本轮重跑。
- 未验证 GDAL 3.7/其他版本、Linux/Windows、muparser 正向计算、全部输出驱动、PostGIS 专库、生产并发、磁盘耗尽或大规模 GEOS 峰值内存。
- 在此次审查结束时，修复建议尚未实施，新的反例尚未转入仓库回归测试。完成修复后，应同时跑对应反例、默认门禁及相关真实数据流程。
- Multica 当前工作区按 `spatial` 名称查询返回空列表，未确认本仓库既有项目绑定；没有向其他项目提交或新建项目，问题保留在本报告。
- 本轮唯一新增的仓库文件为此报告；未改业务源码、提交或推送。现有并行任务改动保留。

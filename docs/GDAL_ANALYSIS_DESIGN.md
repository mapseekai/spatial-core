**GDAL 数据检查、金字塔、栅格计算与矢量分析扩展方案**

更新：2026-11-18。原始源码基线：49c6424。状态：五类能力已实现；本文描述当前 API 和实现边界。执行证据、真实数据结果及未覆盖环境见 [验证记录](GDAL_ANALYSIS_VALIDATION.md)。

本方案承接 [GDAL API 设计](GDAL_API_DESIGN.md) 和 [处理设计](GDAL_PROCESSING_DESIGN.md)，覆盖数据检查与查询、金字塔管理、栅格计算与重分类、矢量多图层叠加、矢量拓扑与覆盖处理。保留当前 Source、同步 Builder、Control、Report、Error 和私有 FFI 分层。

典型使用链是：读取影像信息，确认波段和坐标网格，计算指数并重分类，生成金字塔；读取地块图层，检查几何和覆盖质量，执行区域叠加，必要时清理和简化共享边界。每一步独立调用、独立关闭资源，应用自行决定顺序与中间结果的保存位置。

**1. 总体决策**

| 问题 | 设计选择 |
| --- | --- |
| 公共入口 | 继续使用 raster::操作名、vector::操作名；不新增 analysis 顶层模块 |
| 调用方式 | Builder 拥有 Source 和参数；构造不做 I/O；run(self) / run_with(self, &mut Control) 同步执行 |
| 查询结果 | 新增 operation::Outcome<T>，同时返回类型化值和现有 Report；既有写操作的返回类型不变 |
| 输出行为 | 查询默认只读；计算、叠加和覆盖处理创建新结果；金字塔操作显式修改已有目标或其伴随文件 |
| 原生实现 | 优先现有 GDAL/OGR C API；计算、重分类和覆盖算法使用 GDAL 3.12 的算法 C API |
| 资源与错误 | 复用线程配置、诊断限额、panic-safe 回调、别名保护、checked close 和 OutputState |
| 兼容性 | 保留 GDAL 3.7 基础路径；新符号按构建版本条件编译，运行前检查具体能力 |
| 性能 | 查询默认不全量扫描；栅格由 GDAL 分块执行；覆盖算法明确需要整体加载选定图层 |

不添加通用 Operation trait、算子注册中心、DAG、任务调度或自己的表达式引擎。现有 Translate、Warp、PostGIS writer 不因这次扩展更换公共调用方式。局部复用已有执行逻辑，不先进行全库执行器重构。

**2. 入口和返回值**

| 模块 | 入口 | 返回值 | 主要责任 |
| --- | --- | --- | --- |
| raster | Info | Outcome<RasterInfo> | 元数据、波段、可选统计与直方图 |
| raster | PixelInfo | Outcome<PixelSample> | 像素位置或坐标位置的像元查询 |
| vector | Info | Outcome<VectorInfo> | 图层、字段、几何字段、CRS、可选数量和范围 |
| raster | BuildOverviews | Report | 创建或重建指定金字塔层级 |
| raster | ClearOverviews | Report | 清除指定存储位置的金字塔 |
| raster | RefreshOverviews | Report | 刷新已有金字塔；支持范围刷新时独立进行能力检查 |
| raster | Calculate | Report | 单个或多个栅格的表达式计算 |
| raster | Reclassify | Report | 精确值、区间、NoData 和未匹配值的映射 |
| vector | Overlay | Report | 两个明确图层的叠加分析 |
| vector | CheckGeometry | Outcome<GeometryCheck> | 检查单个要素的几何有效性、可选简单性 |
| vector | CheckCoverage | Outcome<CoverageCheck> | 检查面之间的共享边界、重叠和指定宽度的缝隙 |
| vector | CleanCoverage | Report | 修正共享边界、重叠和指定宽度的缝隙 |
| vector | SimplifyCoverage | Report | 保持共享边界关系的整体简化 |

Outcome<T> 只是一种结果容器，提供 value()、report()、into_parts()，不改变执行机制。查询结果拥有普通 Rust 数据，不返回借用原生 Dataset、Layer 或 Geometry 的对象。读取失败和关闭失败仍返回现有 operation::Error，不将部分查询结果伪装成成功。

新增 vector::LayerSource 表达“数据集中的一个图层”：拥有 Source、图层名称及可选属性/空间筛选。省略名称只允许数据集恰好有一个图层；多图层输入不能静默选择第一个。它供 Overlay 和拓扑操作复用，不强制替换 Translate 现有的多图层/SQL 入口，也不直接复用带 PostGIS 导入语义的 VectorSource。

支持常用参数的类型化方法；驱动创建参数、图层创建参数、算法参数继续分开。已有字段和算法有明确语义时使用枚举，不用多个含糊的 bool。高级原生参数只出现在对应 Native 入口，不能覆盖 Source 的打开选项或 GdalOptions 配置作用域。

**3. 数据检查与查询**

RasterInfo 包含驱动、尺寸、波段、CRS、可选仿射变换、GCP/RPC 标识、块尺寸、像元类型、NoData、scale/offset、颜色解释、mask、overview 列表，以及按需选择的 metadata domain / subdataset 描述。不存在的空间参考或仿射变换使用 Option，不生成伪默认值。不递归打开所有子数据集。

VectorInfo 包含图层名称、属性字段及类型、多个几何字段及各自 CRS/维度、驱动报告的能力。计数和范围使用 ScanMode::Fast / Exact；默认 Fast，只调用不强制扫描的接口，不可快速取得时返回 None，不把未知当作零或空范围。Exact 明确允许扫描。

统计和直方图作为 Info 的可选查询项，合并在一次打开/关闭中执行，不为每种统计重新打开数据集：

- StatisticsMode::CachedOnly / Approximate / Exact；默认不请求统计。结果标明缓存或本次计算，以及近似状态；来源不明的缓存不能标为精确结果。
- HistogramOptions 显式指定范围、桶数、是否包含范围外值及近似模式。桶数、整数转换和内存大小在调用前检查。
- 查询统计使用 GDAL 的明确数值语义。NoData、NaN、独立 mask、alpha 分别测试和说明，不将所有 mask 自动等价为 NoData，也不默默应用 scale/offset。
- 实际计算不能直接触发源文件的统计/PAM 回写。缓存读取不启用 force；计算放在保留源地理信息和有效性语义的受控临时视图上。不能仅关闭整个 PAM，因为源 .aux.xml 可能包含必要的地理信息。统计视图中的每个波段只引用本次已打开的一个源波段；拼接、像元函数、波段顺序和打开选项均由原数据集解释。计算统计不会将请求委托回源文件。统计遵循 GDAL 原生数值语义：排除 NoData，使用存储值，不自动应用 scale/offset；独立 mask 不等价于统计排除条件。PixelInfo 的 valid 单独读取 GDAL mask。当前计算统计/直方图的派生视图经过浮点运算，Int64/UInt64 波段在执行前返回 Unsupported，避免改变大整数的 NoData 归属；这些波段的元数据、缓存统计和精确 PixelInfo 仍可使用。

PixelInfo 使用互斥的 SampleLocation::Pixel / Coordinate。Pixel 为零起点的列、行；波段仍为 GDAL 的一起点。Coordinate 必须声明坐标参考或明确使用源 CRS，按完整六参数仿射逆变换定位；不假定影像无旋转。首版无可逆仿射时返回 Unsupported，GCP/RPC 定位留待专用 transformer 封装。

查询返回实际采样位置、原始值、有效性、NoData 和可选 scale/offset 元数据；不隐式生成可能损失宽整数精度的换算值。越界、NoData、读取失败分别表达；越界不能被当成数值零。整数样本和 NoData 使用能区分 i64/u64/f64 的 PixelValue，避免统一转 f64 丢失大整数精度。复数查询若未实现，显式拒绝。

~~~rust
let result = raster::Info::new("image.tif")
    .bands([1, 2, 3])
    .statistics(StatisticsMode::Approximate)
    .run_with(&mut control)?;
let (info, report) = result.into_parts();

let sample = raster::PixelInfo::new(
    "image.tif",
    SampleLocation::Pixel { column: 120, row: 80 },
)
.bands([1])
.run()?;

let schema = vector::Info::new("land.gpkg")
    .layers(["parcels"])
    .feature_count(ScanMode::Fast)
    .extent(ScanMode::Fast)
    .run()?;
~~~

这轮不增加通用 SQL 查询引擎、任意 Feature 流或全文 JSON 适配层；现有 SQL 转换入口继续工作。底层类型化查询优先使用 gdal / gdal-sys，不依赖解析 gdalinfo / ogrinfo 的文本输出。

**4. 金字塔管理**

金字塔是本轮唯一明确针对已有数据集维护的入口。BuildOverviews、ClearOverviews、RefreshOverviews 的参数名和文档使用 target，避免暗示它们只是读取 Source。需要保留原数据时，由调用方先使用 Translate 复制，再对复制件执行。

| 参数 | 约定 |
| --- | --- |
| storage | OverviewStorage::External / Internal；默认 External，不让驱动静默选择写入位置 |
| levels | 指定缩小倍数；大于 1、唯一、在 GDAL 整数范围内；不要求必须为 2 的幂 |
| bands | 一起点波段；默认全部；驱动不支持子集时在写入前拒绝 |
| resampling | 复用现有 Resampling，仅接受该操作实际支持的变体；缺失变体再按需求补齐 |
| overview_option | 金字塔压缩、块大小等原生选项，和源 open_option、线程配置分离 |
| refresh region | 明确使用源 CRS 的范围或现有层级；不将局部刷新隐式变成全图重建 |

BuildOverviews 对已经存在的指定层级重新计算；ClearOverviews 明确清除所选存储的全部层级，首版不承诺任意选择性删除。RefreshOverviews 只处理已经存在的层级，不隐式创建层级。根据两个轴及实际概览尺寸选择已有层级；奇数尺寸和多个倍率对应同尺寸时同样不得增加概览。全量和局部刷新均须保持层级数量、顺序和尺寸。

~~~rust
let _ = raster::BuildOverviews::new("image.tif")
    .storage(OverviewStorage::External)
    .levels([2, 4, 8, 16])
    .resampling(Resampling::Average)
    .overview_option("COMPRESS", "DEFLATE")
    .run_with(&mut control)?;

let _ = raster::ClearOverviews::new("image.tif")
    .storage(OverviewStorage::External)
    .run()?;
~~~

创建和清除使用 `GDALBuildOverviewsEx`。刷新先按宽、高两轴识别已有物理层，不用推算的层号重建金字塔。全图刷新把选中的概览波段直接交给 `GDALRegenerateOverviews`。范围刷新只复制目标窗口：一般波段通过私有 VRT 和与目标层同尺寸的稀疏临时 GTiff，沿原图网格重采样，并保留源波段实际的 mask（包括来自另一波段的 alpha）。调色板索引波段的 Average、Rms、Mode 改为按完整输出网格定义带色表的 VRT，仅请求目标窗口，使边缘像元也与 GDAL 整层结果一致。临时文件使用 `CPL_TMPDIR` 或系统临时目录。

当前支持普通 GeoTIFF 内置/外置及 VRT 外置金字塔，要求覆盖全部波段且各波段层级尺寸一致。已有的物理 mask 概览随之刷新；不会补建驱动未创建的 mask 层。拒绝 `OVERVIEW_LEVEL` 打开选项和指向其他数据集的外置概览；不支持的驱动返回 `Unsupported`。范围刷新要求非旋转仿射网格，保留窗口外像元和未选层。

必须在进入可能写入的阶段前登记目标及伴随文件。内部模式从 update open 开始即为 MayHaveChanged；外部模式最迟在首次可能创建或修改 .ovr/.aux.xml 等文件前切换状态。已有旁路文件的合法性、存储位置和别名需预检；遇到内部/外部金字塔混合、未知驱动行为或符号链接歧义时明确拒绝，不猜测应更新哪一份。

COG 的内置修改可能破坏布局，首版拒绝，通过 Translate 重新生成 COG；已有内置概览的 COG 也不能绕过混合存储限制建立外置概览。默认 External 也可能覆盖既有金字塔，不承诺只创建新文件。失败不自动删除旧 .ovr、不承诺回滚；需要调用方独占目标，跨进程锁不属于库内保证。GDAL 不支持目标存储方式时返回 Unsupported，不能降级成另一种存储方式。[GDAL 金字塔文档](https://gdal.org/en/stable/programs/gdaladdo.html)

**5. 栅格计算与重分类**

Calculate 与 Reclassify 分开：前者处理数学表达式，后者处理分类规则及边界。使用 GDAL 的 raster calc / reclassify 算法，不引入 Python、NumPy、外部命令或新的 Rust 表达式依赖。

Calculate 使用具名输入：input("A", Source)、input("B", Source)，表达式引用 A[1] 等一起点波段。输入别名必须唯一、非空并满足限定的标识符规则；不把路径、用户字段或配置拼进表达式。支持多个 expression，明确每个表达式和输入波段展开后的输出关系。

~~~rust
// 输入网格已经通过 Warp 对齐；当前 GDAL 必须包含 muparser。
let _ = raster::Calculate::new("ndvi.tif")
    .input("NIR", "nir.tif")
    .input("RED", "red.tif")
    .expression("(NIR[1] + RED[1]) == 0 ? -9999 : (NIR[1] - RED[1]) / (NIR[1] + RED[1])")
    .dialect(ExpressionDialect::MuParser)
    .data_type(DataType::Float32)
    .nodata(-9999.0)
    .nodata_policy(NoDataPolicy::Propagate)
    .creation_option("COMPRESS", "DEFLATE")
    .run_with(&mut control)?;
~~~

类型化计算的首版契约：

1. 默认严格同网格：CRS 语义相同、宽高相同、完整仿射变换一致。仿射只容忍已定义的浮点比较误差，不能吸收半像素偏移。网格不一致先用 Warp 显式对齐，不在 Calculate 中隐藏重采样。单波段广播仅按原生表达式规则发生。
2. 默认 MuParser 表达式；Builtin 使用独立枚举值。能力检查同时验证算法和表达式引擎，缺失时返回 Unsupported，不能替换成另一种方言。表达式不是 Python，也不是 shell。[原生计算语义](https://gdal.org/en/stable/programs/gdal_raster_calc.html)
3. 默认传播原生 NoData；Propagate / EvaluateRawValues 明确区分。按存储的原始像元值计算，不自动应用 scale/offset；需要物理量时在表达式中显式换算或先规范化。输出 NoData 显式设置并按目标类型校验；除零及 NaN/Inf 沿用选定原生表达式函数，不提供全局数值异常策略。支持逐波段标量 NoData。独立 mask、alpha 和由多个波段共同决定有效性的 `NODATA_VALUES` 元组，须先归一化；遇到这些输入会在创建输出前返回 `Unsupported`。例如元组 `0 0` 只把两个波段均为零的像元视为无效；不能把第一波段每个零单独丢弃。
4. 默认浮点计算和 Float64 输出，调用者显式选择 Float32 或整数输出。输入 Int64/UInt64 的精确计算首版不承诺，类型化入口拒绝可能丢失精度的组合；不因输出选 UInt64 就宣称计算过程精确。整数输出沿用 GDAL 的舍入和范围转换，不能宣称无损；精确中间值应保留 Float64。
5. 多输入必须保留各自 allowed_drivers/open_option。优先传递预打开的句柄及具名输入；需要路径的算法使用独占临时目录内的 VRT 代理，不能把不同源的打开选项合并，也不能丢掉选项后重新打开原路径。
6. 首轮类型化输出必须物化。拒绝引用临时代理的 VRT/GDALG 输出，不能在 run 成功后清理临时目录而留下失效结果。以后需要惰性输出时，另行增加持久来源与配置契约。

Reclassify 提供 value、range、nodata 和 unmatched 方法；range 使用 Rust RangeBounds 表达开闭区间，不用含糊的 min/max 两个浮点值。未匹配值用 Unmatched::Error / NoData / Keep / Value；默认 Error。NoData 单独映射，默认保留，不和普通 NaN 数值匹配混为一谈。

~~~rust
let _ = raster::Reclassify::new("landcover.tif", "classes.tif")
    .band(1)
    .value(1, 10)
    .range(2..=5, 20)
    .unmatched(Unmatched::NoData)
    .output_nodata(255)
    .data_type(DataType::UInt8)
    .run()?;
~~~

规则在准备阶段检查：重复精确值、重叠区间、空区间、非法 NaN 端点、输出值不可表示、分类值与输出 NoData 冲突。类型化入口拒绝重叠，不使用规则顺序偷偷决定结果。Builder 支持 Clone；本轮不增加规则 DSL 或独立规则仓库。

Reclassify 只检查实际选中波段的数据类型、NoData 和 mask。其他波段含 UInt64、复数或独立 mask，不影响分类；但若所选波段的有效性取决于数据集的联合 NoData 元组，则不能独立解释，创建输出前拒绝。Calculate 的表达式可以引用绑定数据集的任意波段，因此继续校验每个绑定数据集的所有波段。

PixelValue 保留规则字面量精度；向原生映射序列化时不能先转 f64。原生算法不支持精确分类的数值域在执行前拒绝；第一版以 8/16/32 位整数及经明确边界检查的浮点分类为验收范围。运行时遇到未匹配值仍可能在写入后失败，必须返回 MayHaveChanged。[原生重分类语义](https://gdal.org/en/stable/programs/gdal_raster_reclassify.html)

output_nodata 是本库的组合能力。规则编译将源 NoData 从普通区间中排除，并把 NoData 输入冻结为原哨兵常量（NaN 使用原生 NAN 映射）；输出编译为目标哨兵，在临时结果上设置标记并物化，避免修改派生 VRT 元数据后重新解释 NO_DATA 输入。显式 value(源 NoData, ...) 被拒绝，须使用 nodata(...)。未指定输出哨兵且仍需无效值时，优先使用源 NoData；无源 NoData 的浮点结果使用 NaN，整数结果要求显式 output_nodata。Float32 分类值与哨兵在舍入后再次检查冲突；Keep 与新 output_nodata 的组合被拒绝，避免使原有效值变为无效。

本轮没有新增 NativeCalculate / NativeReclassify；它们是后续按实际需求增加的可选入口。当前 Calculate 支持原生表达式与方言、多个输入和表达式、独立打开选项、输出类型/NoData 和驱动创建选项。类型化验证失败不会自动降级。

**6. 矢量多图层叠加**

使用一个 Overlay Builder 加 OverlayOperation 枚举，覆盖 Intersection、Union、SymDifference、Identity、Update、Clip、Erase；复用 OGR_L_* 图层叠加 C API，不在 Rust 中编写两层要素的笛卡尔积，也不构造大量动态 SQL。[OGR Layer C API](https://gdal.org/en/stable/api/vector_c_api.html)

~~~rust
let parcels = vector::LayerSource::new("land.gpkg")
    .layer("parcels")
    .where_clause("status = 'active'");
let zones = vector::LayerSource::new("zones.gpkg").layer("zones");

let _ = vector::Overlay::new(parcels, zones, "intersection.gpkg")
    .operation(OverlayOperation::Intersection)
    .input_fields(["parcel_id", "area"])
    .method_fields(["zone_id"])
    .field_prefixes("parcel_", "zone_")
    .layer_name("intersections")
    .layer_option("SPATIAL_INDEX", "YES")
    .run_with(&mut control)?;
~~~

一次操作处理两个明确图层；即使它们来自同一个数据集，也分别持有可独立使用的读取游标，避免过滤器和迭代状态互相污染。三个及以上图层由调用者显式分步处理并确定顺序，不引入内置 pipeline。

必须固定以下语义：

- 两侧 CRS 在 Inspect 阶段比较，包含轴顺序和必要的坐标历元信息；缺失或不一致时拒绝。重投影由既有 Translate 完成，不自动选一侧或猜测 CRS。
- 明确区分输入侧和方法侧；Erase / Clip / Update 不能交换参数后声称相同结果。Update 产生新的叠加结果，不是更新原数据集。
- 首批面-面验收覆盖七种操作；点-面、线-面的 Intersection / Clip / Erase 单独验证。其他组合按原生支持和回归矩阵开放，不能笼统宣称支持任意几何组合。
- 首版类型化分析针对 XY 线性几何；Z/M、曲线和不能保持语义的混合几何明确拒绝，调用者先通过 Translate 显式规范化。不偷偷降维或 make-valid。
- Intersection / Union / SymDifference / Identity 默认保留双方字段，以 input_ / method_ 前缀避免同名覆盖；Clip / Erase / Update 保留输入字段；Update 要求选中字段在两侧具有相同名称、类型及子类型。后者不能接受无法实现的 method_fields 配置而静默忽略。
- 显式空投影输出零个业务属性（驱动的 FID/几何列不计入）；非空投影仍映射真实属性值。创建前检查字段选择、前缀和名称冲突；创建后检查驱动实际字段定义，包括 Boolean、JSON 等子类型。叠加会产生重复值与空侧字段，因此不继承原表的 NOT NULL/UNIQUE/default 约束。若驱动截名导致碰撞，必须失败，不把不同字段写进同一列。FID 是输出身份，源 FID 如需保留，使用显式属性列。
- 明确结果几何维度及低维交集的处理：面-面默认仅保留面结果，边界接触不能被算作面面积；需要线/点结果须使用经过验证的选项组合。写入的每个要素都必须符合输出图层的声明；包括 Update 从方法侧追加的要素、同维度多部件，以及只在线或点上接触的面。空结果创建正确 schema 的空图层。
- 默认失败即停止，预检完整输入；固定 SKIP_FAILURES=NO、PROMOTE_TO_MULTI=YES、KEEP_LOWER_DIMENSION_GEOMETRIES=NO，不提供覆盖这些语义的自由 argv。

OGR 图层代数按要素写入一个私有 C++17 输出层适配器：它在写入驱动前剔除低维接触、将同维几何提升为声明的 Multi 类型，并隔离两侧字段的内部映射。GDAL 3.7–3.11 会给空结果层自动补列；适配器在内存中提供一个不匹配任何源字段的定义，向目标驱动只写几何。GDAL 3.12 起直接使用 `ADD_FIELDS=NO`。两条路径都不缓存整层。已知无法保真输出的 Shapefile 几何组合在创建前拒绝；需要回读验证的其他驱动若改写了字段或几何，则返回错误，输出可能已创建。

首轮 Overlay 只创建新本地数据集，默认 GPKG；不同时加入 append、replace-layer 和数据库直写。输出保护调用已有 output.rs 和 target.rs 的适用逻辑。不要给 Overlay 构造假的 TranslateOptions；确有复用的驱动/路径/schema 检查再局部提取。

**7. 矢量拓扑与覆盖处理**

单个几何有效，不代表整个面集合没有重叠或裂缝。因此保留独立入口，不能把现有 Translate::make_valid(true) 或逐要素 simplify 当成覆盖修复或覆盖简化。

| 操作 | 输入与结果 | 默认语义 |
| --- | --- | --- |
| CheckGeometry | 单图层 → 检查摘要及有界问题样本 | 检查有效性；可选检查简单性；NULL、EMPTY 单独计数并按显式策略判定 |
| CheckCoverage | 面图层 → 覆盖摘要及有界问题样本 | 检查重叠、共享边界和显式阈值内的缝隙；不修改源 |
| CleanCoverage | 面图层 → 新面图层 | 显式吸附距离、缝隙阈值、合并归属策略；不自动猜测容差 |
| SimplifyCoverage | 合法面覆盖 → 新面图层 | 显式 tolerance；可选保留外边界；不能修复输入已有的覆盖错误 |

检查返回的 invalid / non_simple / gap / overlap 属于业务结果，不等同于执行失败；打不开数据、读取错误、缺少 GEOS、取消或回调 panic 才返回 Error。GDAL 只返回 invalid edge 而不能可靠归因时，问题类型保留为 InvalidEdge，不推测具体原因。

摘要包含本次检查范围、已检查数量、发现问题数量、有界问题样本和样本是否截断。样本上限只限制保留量，默认仍扫描到结束；执行中断返回 Error，不能将“检查了前 N 条且没发现问题”标为全层通过。问题关联使用源 FID（若存在）加本次读取序号，不保证 FID 连续或全局唯一。

提供可选 issues_output 新路径，将全部问题几何写入可视化图层。默认只返回摘要；先将筛选后的输入流式写入独占 GPKG 快照（设置 CPL_TMPDIR 时使用该磁盘），再使用原生 stream 结果，逐条汇总问题，不把所有问题 Geometry 搬进 Rust Vec。快照保留原 FID 与读取序号以解析原生问题输出；临时字段在写出处理结果前移除。源字段的名称、类型、子类型不能被 GPKG 快照无损表示时明确拒绝；清理/简化的最终输出也检查字段语义。问题图层也执行既有新输出保护和关闭检查。

~~~rust
let source = vector::LayerSource::new("parcels.gpkg").layer("parcels");
let check = vector::CheckCoverage::new(source.clone())
    .maximum_gap_width(0.05)
    .issues_output("coverage_issues.gpkg")
    .run_with(&mut control)?;

let _ = vector::CleanCoverage::new(source, "parcels_clean.gpkg")
    .snapping_distance(0.01)
    .maximum_gap_width(0.05)
    .merge_strategy(MergeStrategy::LongestBorder)
    .run()?;

let _ = vector::SimplifyCoverage::new(
    vector::LayerSource::new("parcels_clean.gpkg").layer("cleaned"),
    "parcels_simple.gpkg",
)
.tolerance(0.5)
.preserve_boundary(true)
.run()?;
~~~

所有距离均为源 CRS 单位，不默认当作米。缺失 CRS、非有限/负数容差均拒绝；非投影 CRS 的正距离须显式 allow_geographic(true)，表示接受源坐标单位，不能把角度当作米。默认吸附和缝隙阈值均为零，SimplifyCoverage 必须给出 tolerance；示例使用投影坐标。先进行单几何有效性检查；CheckCoverage 的前置检查可以与读取合并。CleanCoverage 不替代 MakeValid；SimplifyCoverage 默认验证覆盖有效，发现问题拒绝输出，调用方显式清理后再执行，不默认相信输入已合法。

有效与无效几何均计入已扫描数量，快照阶段定期报告进度，包括不足一批的输入。检查成功返回前再次观察取消；最终回调返回 Continue 但同时取消 token，也返回 Cancelled。写出过的问题图层不保证回滚，错误携带 OutputState。

前置检查会增加扫描和原生运算开销，验收记录必须包含这部分耗时。优先复用已打开输入，不假定 GDAL 可以复用两个算法内部的几何集合，也不同时保留两份全层原生结果。检查通过与后续处理之间仍要求调用方保证输入不被其他进程修改。

LayerSource 的过滤器限定被分析的完整集合。结果明确记录检查范围；对筛选子集的通过不表示原完整图层通过。保留外边界可以避免简化子集时改变它与外部数据的边界，但不能据此承诺检查了未加载的相邻面。

覆盖算法需要整体处理选定图层。预算参数可限制输入要素数/顶点数，超限拒绝；这属于输入规模限制，不是 GEOS RSS 硬限制。不能按窗口独立处理后拼接并声称拓扑等价。共享边界跨窗口需要另一个经过证明的算法，首轮不做。

检查、清理、简化分别保留原生语义和版本条件：CheckCoverage / SimplifyCoverage 需要 GEOS >= 3.12，CleanCoverage 需要 GEOS >= 3.14；算法本身来自 GDAL >= 3.12。清理会改变边界与面积，合并策略是调用方可见的业务选择；简化保持共享关系但不保证面积严格不变。[检查](https://gdal.org/en/stable/programs/gdal_vector_check_coverage.html)、[清理](https://gdal.org/en/stable/programs/gdal_vector_clean_coverage.html)、[简化](https://gdal.org/en/stable/programs/gdal_vector_simplify_coverage.html)

**8. 原生版本与资源契约**

| 能力 | 实现路径 | 版本/能力条件 |
| --- | --- | --- |
| Info、PixelInfo、基础统计 | gdal / gdal-sys 读取接口 | 保持当前 GDAL 3.7 基线；具体驱动可缺少部分信息 |
| Build/ClearOverviews | GDALBuildOverviewsEx | 基线 3.7；另检查驱动、存储方式及重采样支持 |
| 全量刷新 | GDALRegenerateOverviews | 写入选中的既有物理层 |
| 范围刷新 | 原生 overview refresh；调色板 Average/Rms/Mode 经 VRT RasterIO | 封装的范围刷新入口要求 GDAL 3.12；两条路径都只回写选中层的窗口 |
| Calculate、Reclassify | 私有 Algorithm RAII | GDAL >= 3.12 且算法/方言可用 |
| Overlay | OGR_L_Intersection / Union / SymDifference / Identity / Update / Clip / Erase | GDAL 3.7 基线，需 GDAL 链接 GEOS |
| CheckGeometry | OGR 几何检查接口 | GDAL 3.7 基线，需 GEOS；结构化原因在原生支持范围内提供 |
| Check/SimplifyCoverage | GDAL Algorithm C API | GDAL >= 3.12、GEOS >= 3.12 |
| CleanCoverage | GDAL Algorithm C API | GDAL >= 3.12、GEOS >= 3.14 |

GDAL 3.11 的新命令体系不意味着 3.11 已有统一算法 C API；该 C API 从 3.12 开始提供。[官方 C API 使用说明](https://gdal.org/en/stable/programs/gdal_cli_from_c.html)

build.rs 继续读取 DEP_GDAL_VERSION_NUMBER，并为新增符号发出明确 cfg 与 check-cfg；旧构建中保留公共 Builder，执行时返回 Unsupported，不能引用未存在的原生符号。当前已启用 bindgen，先检查依赖生成的绑定是否包含 gdalalgorithm.h；缺失时优先使用上游绑定支持，Algorithm 的局部 C 声明只放私有 FFI 模块。Overlay 单独使用公开的 OGR C++ 图层接口接收输出要素；Rust 边界仍为 C ABI。该适配器要求 C++17 编译器、GDAL 开发头文件，并在运行前检查头文件与运行库的 GDAL 主、次版本相同。

构建版本控制符号是否能编译，运行版本/算法注册情况/GEOS 版本控制能力是否可用，两者不可互相替代。还要检查所需参数和方言是否可用，算法名称存在不代表所有参数均受支持。编译链接了新符号的二进制仍要求匹配的运行库，不能靠 if(version) 保证它能在旧动态库上启动；本方案不新增 dlopen 兼容层。现有 README 已记录 GDAL 3.13.3 的编译问题，因此“3.12+”是功能下限，不是所有后续版本都已验证。

私有 Algorithm 包装只承载本轮确实使用的算法与参数，负责 registry、algorithm、argument 和 dataset-value 句柄的所有权。输出借用句柄不能直接包装成拥有所有权的 Dataset，也不能和 Finalize 重复关闭；只有明确转移/增加引用的句柄才交给 Rust Dataset 管理。

执行顺序保持：参数与能力预检 → 配置/诊断作用域 → 打开输入 → 数据检查 → 输出保护 → 执行 → 完成原生输出/Finalize → 关闭输入 → 恢复配置 → 收集报告。AlgorithmRun 失败也要执行适用的 Finalize/Release 清理，Finalize 的返回值及 CPL 错误均要检查，保留主错误和独立清理错误；不能仅依赖 Drop 报告写入完成。

算法可能延迟读取输入，所有输入、代理 VRT、配置和临时目录必须活到输出最终完成之后。Progress 到达 100% 不表示完成。无原生回调的阶段只能在调用前后检查取消，不承诺强制中断；FFI 不允许 Rust panic 穿越。

查询执行器复用现有控制/诊断/close 原语；金字塔维护使用独立的目标状态管理。raster::execute::Job 继续服务创建新输出，不能添加一串 bool 来兼容查询、修改和多图层写入所有场景。

**9. 文件组织与影响范围**

以下是实际实现文件。已有小模块保持平铺；原生调用只有少量适配代码时直接放在对应操作模块，不增加空壳层。

~~~text
src/
  operation.rs                  # Outcome<T>、ScanMode；原有 Report/Error/Control 保持不变
  dataset.rs                    # Source 及私有的 update-open、打开选项读取
  gdal_ext/
    query.rs                    # 只读执行、借用回调与检查后的关闭
    algorithm.rs                # GDAL 3.12 C API 声明、RAII、参数与能力检查
    execution.rs / output.rs    # 复用现有关闭、版本、文件别名保护
  raster/
    info.rs                     # Info、统计/直方图与拥有所有权的结果
    pixel_info.rs / value.rs    # 仿射坐标查询、精确 PixelValue
    overviews.rs                # Build/Clear/Refresh、维护状态与直接 C API
    calculate.rs                # 具名输入、方言、网格预检、临时 VRT 与物化
    reclassify.rs               # RangeBounds、NoData 编译、目标类型检查
    execute.rs                  # 复用 Job，增加私有 scratch/materialize 适配
    mod.rs                      # 显式导出
  vector/
    info.rs / source.rs         # schema/计数/范围、LayerSource
    overlay.rs                  # 七种 OGR 叠加、字段投影与落盘校验
    overlay_sink.cpp            # 按要素归一化后写入真实驱动；C ABI
    check_geometry.rs           # 几何有效性、简单性、问题输出
    check_coverage.rs           # 覆盖检查 Builder
    clean_coverage.rs           # 覆盖清理 Builder
    simplify_coverage.rs        # 共享边界简化 Builder
    coverage.rs                # 三个覆盖操作共用的快照、预算、原生执行与报告
    execute.rs                 # 多输入/新输出执行、GEOS 与 schema 检查
    analysis_test_support.rs    # 私有测试 fixture
    mod.rs                      # 显式导出
build.rs                        # gdal_3_12 cfg / check-cfg
examples/
  inspect.rs                    # 可单独查询真实 raster/vector
  raster_analysis.rs            # 真实影像窗口复制后进行全部栅格分析
  vector_analysis.rs            # 检查、叠加、覆盖清理与简化
scripts/ci/check.sh              # 默认检查包含上述三个示例
docs/GDAL_ANALYSIS_VALIDATION.md # 实际验证结果与明确未执行项
~~~

检查报告的共享结果类型先放在实际使用它的检查模块，需要跨检查复用时再局部提取，不先增加通用 report 框架。gdal_ext 子模块中的适配若只有一两行直接调用，可保留在对应操作实现中，避免为文件树凑文件。

新增模块需要触及相应 mod.rs 的导出、私有测试、examples、scripts/ci/check.sh、README 和 TESTING.md。修改共享 execution/progress/ffi 时必须回归所有既有 raster/vector 操作；PostGIS 低层 Warp 等调用保留，若实际改到这条链路，再执行专用数据库回归。新示例的高版本能力必须明确报不支持，不能静默跳过却计为成功。

**10. 实施顺序与完成条件**

| 阶段 | 工作 | 完成条件 |
| --- | --- | --- |
| P0：验证原生边界 | 检查绑定/ABI、算法存在性、具名输入句柄、不同 Source 打开选项、Finalize；验证只读统计不会回写源；验证 overview 内外置目标 | 小型真实 fixture 证实上述路径；确定兼容矩阵，不能只靠文档推断所有权 |
| P1：查询 | Outcome、Info、PixelInfo；Fast/Exact、统计和直方图 | 默认不强制扫描；计数/像元/元数据准确；源和伴随文件不被查询修改 |
| P2：金字塔 | Build/Clear、全量刷新，再范围刷新 | 关闭重开后验证层级与像元；失败状态正确；COG/内外置/伴随文件保护成立 |
| P3：计算 | Algorithm 最小 RAII、Calculate、Reclassify、所需 Native 入口 | 实际像元正确；网格/NoData/规则边界/数值精度成立；无临时依赖泄漏 |
| P4：叠加 | LayerSource、Overlay 七种操作、字段与几何类型策略 | 要素/属性/面积和方向性正确；不静默跳过；空结果与同源双游标正确 |
| P5：拓扑 | CheckGeometry、CheckCoverage、CleanCoverage、SimplifyCoverage | 版本/GEOS 门槛可测；检查结果与执行错误分离；共享边界和问题定位正确 |
| P6：集成 | 示例、文档、版本矩阵、真实数据和完整回归 | 每项能力分别记录实测结果和未覆盖环境；不把高版本跳过当通过 |

P0–P6 已按上述顺序实现。可选 Native 入口没有加入；旧 GDAL、其他操作系统及原生可选表达式引擎的验证范围单列于验证记录，不将实现中的版本分支等同于所有环境都已实测。

**11. 回归与真实数据验收设计**

私有测试继续放对应源码文件，不建立一个汇集所有新操作的巨型测试文件。只为公共组合使用独立集成测试，并遵循现有源码镜像和一份独立测试文件约定。

| 领域 | 必须覆盖的真实 GDAL fixture |
| --- | --- |
| 查询 | 带/不带 CRS、旋转仿射、GCP/RPC、PAM 地理信息、UInt64、NoData/NaN/mask、多个几何字段、空层、Fast 不可得、精确扫描读取失败 |
| 金字塔 | 内外置创建、已有层级重建、全清、范围刷新、分类 Mode/连续值 Average、COG 拒绝内改、旧 .ovr 和路径别名、取消后的可观察状态 |
| 计算 | NDVI、小型多输入多输出、单波段广播、半像素偏移、相同范围不同网格、CRS 不同、不同 open_option、缺引擎、NaN/除零/类型边界 |
| 重分类 | 开闭区间端点、重叠拒绝、负值、未匹配策略、输入/输出 NoData、宽整数拒绝、映射精度、输出关闭后的真实像元 |
| 叠加 | 七种操作、输入方向、共享/冲突字段、字段截名、无效几何、空结果、点/线与面、同数据集两层及同层两组筛选 |
| 拓扑 | 自交面、简单性、NULL/EMPTY、重叠、共享边不匹配、小缝/合法孔洞、不同合并策略、外边界保持、过滤子集、样本截断和实际读取失败 |
| 资源 | 预取消/执行中取消、回调 panic、算法失败后的 Finalize、关闭错误组合、参数不支持时 NotModified、临时目录生命周期、配置和 CPL 状态恢复 |

兼容性目标是 GDAL 3.7 的基础路径；本次实际环境为 GDAL 3.12.2 / GEOS 3.14.1，尚未运行独立 GDAL 3.7 环境。每个不支持分支也是测试项，但不能替代能力本身的正向测试。

真实数据从桌面已有测试目录选择 GeoTIFF/COG、分类栅格、Shapefile/GPKG 面数据。所有维护测试使用复制件；其余操作写独占临时目录。记录数据尺寸、要素规模、GDAL/GEOS 版本、耗时与读回核对；峰值内存尚未采样。不能把小样本表现当成百万面的性能保证。

运行仓库要求的 bash scripts/ci/check.sh；新增高版本例子和测试的 CI 选择要显式区分兼容性任务与完整能力任务。若共享底层调用影响 PostGIS，按 TESTING.md 单独跑专用数据库回归。六项修复已完成真实数据复验及两名独立审查者的四轮交叉检查，发现、修订和验收范围见[分析验证记录](GDAL_ANALYSIS_VALIDATION.md#四轮对抗复审)。

**12. 本轮边界与待验证风险**

本轮不规划多维数组、XYZ/TMS 瓦片、通用 GDAL 命令执行器、自动投影/网格对齐、拓扑工作流、任意几何关系规则语言或新的 PostGIS 写入模式。

原生边界回归覆盖独立打开选项、增加输出引用后的 Finalize、统计/PAM 只读和明确驱动的概览行为。剩余环境差异包括原生引擎构建选项、驱动实现与大规模 GEOS 内存；详见验证记录。输入内容导致的运算失败仍可能留下部分结果，金字塔维护可能改变已有目标；保持现有诚实的 OutputState 契约，不引入虚假的事务保证。

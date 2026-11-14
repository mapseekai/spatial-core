# GDAL 栅格与矢量处理设计

日期：2026-11-14。基线：`3b6c099`。本设计延续 [矢量转换设计](GDAL_API_DESIGN.md) 的输入、控制、错误与资源契约，扩展栅格和矢量处理；不引入工作流引擎或算子注册框架。

## 1. 能力和入口

| 领域 | Builder | 本次能力 |
| --- | --- | --- |
| 栅格转换 | `raster::Translate` | 格式、波段、像元类型、窗口、尺寸、重采样、NoData、创建选项 |
| 重投影和拼接 | `raster::Warp` | 多输入、源/目标 CRS、范围、分辨率或尺寸、重采样、NoData、cutline、warp/transformer 选项 |
| 虚拟拼接 | `raster::BuildVrt` | 多输入、拼接/波段堆叠、分辨率、范围、NoData；源文件必须在使用 VRT 期间继续存在 |
| 地形 | `raster::Dem` | hillshade、slope、aspect、TRI、TPI、roughness、color-relief；波段、边缘、比例和算法参数 |
| 矢量栅格化 | `raster::Rasterize` | 图层/SQL/属性筛选、常量或属性烧入、范围、尺寸/分辨率、NoData、初始化值 |
| 分类矢量化 | `raster::Polygonize` | 波段、四/八邻域、有效性掩膜、整数/浮点类别、图层和字段、输出驱动选项 |
| 等值线 | `raster::Contour` | 波段、间隔/固定高程、基准高程、NoData、三维线、图层和高程字段 |
| 填洞 | `raster::FillNodata` | 先复制到新输出，再填指定波段；搜索距离、平滑、掩膜及算法选项 |
| 小区域过滤 | `raster::Sieve` | 先复制到新输出，再过滤指定整数波段；阈值、邻接与掩膜 |
| 矢量处理 | `vector::Translate` | 现有过滤、SQL、CRS、维度、make-valid，加源/目标裁剪、简化、顶点加密、集合拆分、几何类型转换 |

有 utility API 的操作另提供 `NativeTranslate`、`NativeWarp`、`NativeDem`、`NativeRasterize`、`NativeBuildVrt`。原生参数是 argv 元素，不是 shell 字符串。高级参数不能偷偷覆盖类型化 builder 的契约。

## 2. 一致的调用方式

```rust,no_run
use spatial_core::{dataset::Source, raster::{Translate, Warp, Resampling}};

# fn example() -> spatial_core::operation::Result<()> {
let _ = Translate::new(Source::new("image.tif"), "image.cog.tif")
    .driver("COG")
    .bands([1, 2, 3])
    .creation_option("COMPRESS", "ZSTD")
    .run()?;

let _ = Warp::new(["west.tif", "east.tif"], "mosaic.tif")
    .target_srs("EPSG:3857")
    .resolution(10.0, 10.0)
    .resampling(Resampling::Bilinear)
    .creation_option("TILED", "YES")
    .run()?;
# Ok(()) }
```

所有 builder 拥有输入描述和选项，构造不打开文件；`run(self)` 或 `run_with(self, &mut Control)` 执行一次同步工作并返回 `operation::Report`。借用的回调为 `FnMut(&Progress) -> ControlFlow<()> + Send`，取消、panic 隔离、诊断限额、关闭错误与矢量转换保持一致。

栅格算法不把全图读入 Rust 的 `Vec`，使用 GDAL 的分块 I/O。多输入 Warp/VRT 持有输入句柄直到输出关闭，内存与文件描述符随输入数量增长。每次打开下一个输入前检查取消，已打开的输入仍统一关闭；单次阻塞 I/O 不支持强制中断。没有进度回调和取消 token 时不安装原生回调，存在任一控制项时保留串行回调、panic 和 CPL 状态隔离。调用者把同步任务放到自己的 blocking 线程并控制并发量。

## 3. 参数分层

1. `Source::allowed_drivers/open_option` 只控制该输入的打开；栅格或矢量打开标记由操作选择。同一 `Source` 不携带打开的句柄。
2. `GdalOptions` 是整个执行、关闭期间的线程配置作用域。原生 argv 禁止 `-oo`、`-if` 和全局配置类参数。
3. 常用参数使用枚举、数组、数值和具名方法。尺寸、分辨率、范围、band index 在写入前校验；互斥选项报错，不采用最后一个静默覆盖不同语义。
4. `creation_option`、`layer_option`、`warp_option`、`transformer_option`、算法选项分别对应 GDAL 阶段；键不能包含分隔符、空白或 NUL。
5. 不把所有 GDAL 参数穷举成 Rust 枚举。Native 入口接受用户显式承担的 GDAL 语义，并保留统一的输入/配置/资源控制。

## 4. 输出和失败契约

类型化栅格操作只创建不存在的本地输出。已知驱动检查主文件和已知伴随文件；未验证的多文件驱动要求独占的空输出目录。源和输出路径、硬链接、软链接及 GDAL 报告的文件集合不能重叠。检查不承诺跨进程原子互斥：调用者必须独占目标目录。

FillNodata/Sieve 的类型化接口总是先复制源，再修改输出，避免在取消或失败时破坏源文件。现有接受 `&mut Dataset` 的低层入口保留其原地修改语义，文档明确区分。不会自动删除失败输出；`OutputState::MayHaveChanged` 表示可能有部分数据，重试前由调用者处理。

Native 入口允许原生输出名称和原生覆盖规则，但仍检查可识别的本地输入/输出别名。它不保证任意 VSI、数据库连接字符串和外部插件的副作用隔离。

驱动名称先由 GDAL 解析为标准名称，再检查伴随文件，大小写不能绕过保护。别名预检仅在一次执行内缓存输入文件及目标文件的规范化路径、类型和 Unix 文件身份；驱动补充的新目标仍与所有已知输入比较，保留扩展名与实际驱动所需目标的并集，以及同路径、软链接、硬链接和目录包含保护。每个数据集文件列表只读取一次，文件身份缓存内存随来源文件数增长，不跨任务缓存；调用方仍需独占输出目录，预检不提供跨进程的原子性。DEM 颜色表、Warp 的可识别本地 cutline 及其数据集文件也参与别名检查。COG 与 FillNodata 使用独占临时目录，避免原生算法覆盖或删除可预测名称的文件；若设置 `CPL_TMPDIR`，在该目录下创建独占子目录，否则优先使用本地输出目录。配置与目录保留到输出及输入关闭后，取消和 panic 路径也恢复、清理。临时目录需要可用磁盘空间，路径须为 UTF-8。

GDAL utility 返回的句柄只包装一次。输出先关闭，所有输入再关闭，最后恢复配置并收集诊断。关闭时同时检查返回值与本次调用的 CPL 错误状态，避免 VRT 延迟写入失败却返回成功；此前已处理的 CPL 错误不计为关闭失败。关闭失败保留独立来源，不掩盖主要错误。`run` 返回成功才表示输出关闭已检查；回调到达 100% 不表示持久化成功。需要构建和运行 GDAL >= 3.7。

## 5. 算法的具体语义

- Translate 改 CRS 标签与 Warp 重投影分开；Translate 的输出尺寸是像元数，源窗口是像元坐标。
- Warp 多输入按用户顺序处理，重叠规则沿用 GDAL；尺寸与分辨率互斥。VRT 不复制像元，它依赖源文件、其驱动和读取配置；不能把临时源移除后再使用 VRT。`BuildVrt` 和 `NativeBuildVrt` 在写入前拒绝包含 `Source::open_option` 的输入，返回 `Unsupported / Prepare / NotModified`：当前句柄接口不能可靠持久化每个来源的打开选项。可先逐来源调用 `Translate::new(source, proxy).driver("VRT")`，再将持久化的代理 VRT 交给 BuildVrt；代理和原始来源都须保留。不会将不同来源的选项合并成全局选项。
- DEM 的 color-relief 需要显式颜色表；其他模式拒绝颜色表。没有独立重投影或垂直单位推断层，比例由参数及所用 GDAL 版本决定。
- Rasterize 明确选择常量烧入或属性烧入，两者互斥。`assign_srs` 仅改输出 CRS 标签；需要重投影时先执行矢量转换或随后 Warp。创建新栅格必须提供尺寸或分辨率；NoData 与初始像元值分开配置。
- Polygonize 默认按 band 的 validity mask 排除无效像元。整数模式走 `GDALPolygonize` 的 Int64 通道；浮点模式走 `GDALFPolygonize`，存在原生 Float32 精度边界。输出 CRS 来自源，缺失 CRS 时不伪造。
- Polygonize/Contour 使用实际新增字段的位置，不依赖驱动保留原字段名；Shapefile 缩名警告仍记录在报告中。
- Contour 默认读取 band 的 NoData，支持显式忽略；只直接接受全有效或单波段 NoData 掩膜，多波段 `NODATA_VALUES` 联合掩膜也需先归一化，或显式 `ignore_mask(true)` 承担忽略语义。固定高程与间隔互斥。
- Sieve 接受有符号/无符号 8/16/32 位整数，不将浮点类别静默截断。FillNodata/Sieve 的 GTiff 复制要求所有输入波段类型相同，混合类型 VRT 在创建输出前拒绝；其他波段保持复制结果。
- FillNodata 的原生 Float32 工作缓冲会改写有效像元。因此实际插值仅接受 Float32、8/16 位整数，宽类型须由调用者先显式转换；默认全有效掩膜或 `Mask::None` 仅复制，不经过插值缓冲。独立有效性掩膜仍保留复制值，不承诺填洞后自动把该掩膜改为有效。复制与算法各自报告进度，fraction 可能重新从零开始。
- 所有类型化输入波段及 `Mask::Band` 共用边界校验：零和原生整数溢出返回 `InvalidArgument / Prepare`，超出已打开源的波段数返回 `InvalidArgument / Inspect`，均为 `NotModified`；真实波段访问失败保留原始错误，归为 `Processing / Inspect`。
- `Mask::Band` 必须指定 UInt8 波段，保证零无效、非零有效；拒绝 GDAL 隐式 Byte 转换会改变有效性的类型。算法仍受 GDAL 对 scale、offset 和目标驱动的原生行为约束。
- 矢量源裁剪、简化和顶点加密使用源坐标单位，目标裁剪在重投影后使用目标坐标单位。方法的调用顺序不改变 GDAL 的固定处理顺序。需要另一种顺序时显式分成多个输出步骤。
- 集合拆分可能增加要素数并重新分配 FID。改变几何类型的操作不隐式放宽追加 schema 校验。Buffer、dissolve 和属性计算使用现有 `sql(..., SqlDialect::Sqlite)`，由 GDAL 的 SQLite/GEOS 能力决定；不拼接未经引用的字段名。
- 具体 `GeometryType`（如 `MultiPolygon`）必须同时指定 `Dimensions`，并生成对应 `-nlt` Z/M/ZM 后缀，避免只设置 `-dim` 仍静默降维。`PromoteToMulti`、`ConvertToLinear`、`ConvertToCurve` 省略维度时保留原生维度。

兼容性：保留现有借用 Dataset 的低层转换函数，PostGIS 继续复用已有 Warp 调用链；`DemMode` 统一到新增非穷尽枚举并增加 `ColorRelief`，下游穷尽匹配需增加兜底分支。本轮属于用户授权的完整重构，不承诺所有公共类型的源码兼容。

## 6. 文件组织

```text
src/
  dataset.rs                 # Source、open flags 与受校验的键值参数
  operation.rs               # 公共 Control / Report / Error
  gdal_ext/
    execution.rs             # 共同版本、关闭与诊断收尾
    output.rs                # 本地路径与数据集文件集合保护
    raster/
      utility.rs             # 原生 utility options RAII、返回句柄与 usage error
    progress.rs              # 已有 panic-safe 回调桥接
    diagnostics.rs           # 已有作用域诊断
  raster/
    options.rs               # Resampling、Connectivity 等共享类型
    execute.rs               # 栅格路径操作的统一执行与输出预检
    translate.rs             # 栅格转换 Builder
    warp.rs                  # 多输入重投影 Builder
    dem.rs                   # 地形 Builder
    rasterize.rs             # 矢量到栅格 Builder
    vrt.rs                   # VRT Builder
    native.rs                # 原生 argv 入口
    polygonize.rs            # 分类像元转面
    contour.rs               # 等值线
    fill_nodata.rs            # 复制后填洞
    sieve.rs                 # 复制后小区域过滤
    convert.rs               # 保留现有借用句柄的低层接口及其回归
    tiling.rs                # 既有窗口规划
  vector/
    translate.rs             # 复用转换执行器承载矢量处理
    options.rs               # 增加具名的几何处理参数
    target.rs                # 原有矢量 schema 与写入模式检查
examples/
  raster_convert.rs
  raster_process.rs
  vector_convert.rs
  vector_process.rs
```

私有测试放在对应模块。`execution` 和 `output` 仅承载栅格与矢量确实共享的资源逻辑；不引入 trait 注册、动态算子或自制 pipeline。

## 7. 验收

每种能力生成微型真实 GDAL fixture，关闭并重新打开输出，核对像元、CRS、仿射变换、要素或几何结果。覆盖非法参数、输出碰撞、同源别名、预取消、执行中取消、回调 panic、多源关闭、诊断恢复和 Native 参数边界。原有矢量与 PostGIS 依赖的栅格路径继续回归。

运行 `bash scripts/ci/check.sh`。使用桌面已有数据在临时目录验证转换/重投影/处理；源数据不修改。记录使用的 GDAL 版本、执行的测试和未覆盖项。实现完成后由两位 Astra max 独立审查 API/参数语义与 FFI/数据安全，修复确认问题并复验。

## 8. 后续接入

Grid、Nearblack、Viewshed、多维数组、overviews、栅格计算等能力按同样的操作模块接入；本次不承诺包装所有 GDAL 算法。GDAL 3.11+ 的统一 `gdal` 算法 API 不成为本轮最低版本依赖。需要其能力时再单独设计运行时能力探测，避免把版本差异藏进通用字符串执行器。

## 9. 核对来源

- [GDAL utility C API](https://gdal.org/en/stable/api/gdal_utils.html)：选项生命周期、进度回调与句柄返回。
- [GDAL algorithms C API](https://gdal.org/en/stable/api/gdal_alg.html)：polygonize、contour、fill-nodata 和 sieve。
- [ogr2ogr](https://gdal.org/en/stable/programs/ogr2ogr.html)：裁剪、简化、加密及集合拆分的原生语义。
- Context7 `/osgeo/gdal` 与本地 GDAL 3.12.2 头文件；版本特有能力以构建/运行能力为准。

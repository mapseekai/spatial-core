# GDAL 封装实现与验证记录

日期：2026-11-13。设计见 [GDAL_API_DESIGN.md](GDAL_API_DESIGN.md)。实现基于 `1b6b8fd`，保留并整合了并行任务已提交的取消、路径编码和 PostGIS 修复。

## 交付

- `dataset::Source`：拥有路径/显式 GDAL 名称、输入 driver 限制和 open options；运行时打开输入，Unix 路径保留字节。
- `vector::Translate`：消费式 builder，四种明确写入模式，字段/SQL/过滤/CRS/维度/几何修复及各阶段 driver options。
- `vector::NativeTranslate`：独立原生参数入口；共用打开、控制、诊断、FFI 和关闭执行器。
- `operation`：借用式 `Control`、`Progress`、有界诊断、类型化错误、输出状态及分别保留的关闭错误。
- raster 共用串行化的 `Send` 回调桥接；PostGIS fixture 已迁移；旧格式专用入口已删除。新增 `vector_convert` 示例并加入完整检查。

没有新增 Cargo 依赖，也没有引入通用 Operation trait、异步执行框架、线程池或未来 raster builder 的空实现。保留第三方声明。

## 环境及自动检查

本机 macOS ARM64，Rust 1.98.1，gdal 0.19.0 / gdal-sys 0.12.0。独立 C probe 核对了 headers、运行库均为 GDAL 3.12.2，实际加载同一 Homebrew keg 的 `libgdal.38.3.12.2.dylib`。PROJ 使用与本机 native 安装匹配的数据；未修改全局 symlink 或 Cargo 配置。

仓库的 `check-gdal.sh` 是 conda 单 prefix 布局校验，本地 Homebrew 的 PROJ 位于单独 prefix，不能把该脚本未通过记为通过；上述 C probe 单独核实了实际 headers/库版本与加载路径。

检查结果：

| 检查 | 结果 |
| --- | --- |
| `bash scripts/ci/check.sh` | 结构、格式、Clippy `-D warnings`、默认测试、Rustdoc、两个示例及 diff 检查通过 |
| 默认 Rust 测试 | 64 项通过；26 项有意 ignored，另行执行如下 |
| Doctest | 3 项通过 |
| 可选 GeoParquet | 已安装 Parquet driver，单独执行 ignored 读回测试通过 |
| PostGIS 回归 | 专用临时库的 25 项全部通过，无 ignored |
| 双模型审查 | 两位 `gpt-6-astra`、`max`；修复后的独立公开 API 探针复核通过 |

PostGIS 使用本机已存在镜像创建独立临时容器，仅监听 loopback：PostgreSQL 17.7、PostGIS 3.6.1、服务端 GDAL 3.10.3。合成 OutDB fixture 通过相同绝对路径只读挂载。验证了 vector/raster、取消、事务、trigger、故障代理和元数据；测试容器及其测试卷已清理，未改其他容器或业务库。

默认测试涵盖 GPKG、GeoJSON、FlatGeobuf、Shapefile 往返、PGDUMP 文本、空表/无几何表、NULL/大整数/时间字段、Z/M/ZM/EMPTY、混合几何、显式 make-valid/skip-failures、写入模式、CRS、特殊字段名、配置恢复、诊断上限、回调取消/panic。`warp -multi` 回归实际观察到多个 native 回调线程，并证明用户已处理的 GDAL 错误不会被重放为外层失败。

关闭故障用最小私有 `record_close` 接缝验证：主失败与输出、输入两个关闭失败都保留。它不代表已经向每个真实驱动注入磁盘耗尽或设备关闭故障。

## 双模型审查及修复

合并同根因项后共六类问题，均已修复并复核：

| 级别 | 实测问题 | 修复与验证 |
| --- | --- | --- |
| P1 | `LAUNDER:` / `OVERWRITE:` 利用 GDAL 的冒号分隔规则绕过 WriteMode | 选项键拒绝分隔符和空白；目标原有记录保留 |
| P1 | Shapefile 单文件视图漏查旁侧图层，`a/b` 被清洗为已有 `a_b` 后覆盖 | CreateLayer 必须指向目录；更新名限字母数字、`_`、`-`；写前检查伴随文件 |
| P1 | `.shp`→所在目录/`.dbf`、共享 `.dbf` 硬链接、DBF-only→同名 `.shp` 修改输入 | 比较输入 `GDALGetFileList` 与已知目标文件集合，检查 canonical 路径和 inode；Typed/Native 均拒绝，源文件字节不变 |
| P1 | 多几何列追加的预检映射与 GDAL 实际映射不一致，出现 NULL 几何或错 CRS 坐标 | 要求多几何列同名同序且名称唯一，再比较类型/维度/CRS；拒绝重排，合法属性列重排仍成功 |
| P2 | 把同 stem 的独立 GeoJSON 误当作 Shapefile 伴随文件 | 限定实际伴随后缀；`roads.geojson`→`roads.shp` 成功且源字节不变 |
| P2 | `CPL_DEBUG=ON` 下 config set/unset 警告漏收，并覆盖调用前 CPL 状态 | 诊断 guard 包围配置完整生命周期；报告含两次警告，调用前错误号/消息恢复 |

另外按原生契约修正更新调用：已有 `hDstDS` 时传 `pszDest=null`。独立对抗探针验证了 panic payload 的析构再次 panic 时仍返回 `CallbackPanicked`，不会跨 C ABI unwind。两位审查者在各自范围内均报告没有剩余阻断项；这不等于所有平台和所有驱动已经验证。

## 桌面真实数据

全部输入来自本机桌面“测试数据”目录，只读打开；输出进入新临时目录。8 组数据的源文件大小与修改时间均未变化。成功结果重新打开核对要素数，检查字段和几何类型；两个大数据样本还通过保留的 `fid` 属性定位 20 条记录，比较属性及几何 WKB。

| 输入 | 输出 | 要素数 | 结果/参数 |
| --- | --- | ---: | --- |
| `point.geojson` | GPKG | 8 | 通过 |
| `line2 2.geojson` | FlatGeobuf | 385 | 通过；2 个属性字段保留 |
| `测试的shp数据3857.shp` | GPKG | 385 | 通过；字段类型和 Polygon 声明读回一致 |
| `全球国家边界.fgb` | GPKG | 177 | 通过；168 个属性字段保留 |
| `全球国家边界21212.parquet` | GPKG | 177 | 通过；168 个属性字段保留 |
| `test.gpkg` | GeoJSON | 1 | 通过 |
| `building.geojson`，约 112 MB | GPKG | 251,014 | 默认转换通过；另以 `FID=row_id` 验证普通 `fid` 字段及几何样本保留 |
| `big.fgb`，约 84 MB | GPKG | 251,014 | 默认转换明确失败；显式 `FID=row_id` 后通过，字段和样本读回一致 |

`big.fgb` 的内部 FID 与普通 `fid` 字段值不同，原生 GDAL 3.12.2 的 `ogr2ogr` 也复现 `Inconsistent values of FID ... and field of same name`。封装保留失败和诊断，未自动丢列或启用 skip-failures。调用方可明确选择：

```rust,ignore
let report = Translate::new(input, output)
    .driver("GPKG")
    .layer_option("FID", "row_id")
    .run()?;
```

这会分开目标内部 FID 与输入普通 `fid` 属性，不保证跨格式保持原有内部 feature ID。已加入对应合成回归。

一次本机 debug 构建观察：显式 FID 的大 FlatGeobuf 转换及读回约 0.93 秒，进程最大 RSS 约 92 MiB；大 GeoJSON 约 5.25 秒，最大 RSS 约 1,921 MiB。计时和内存包含验证程序的输入/输出重开及样本检查，不能当作转换函数单独的内存上限或服务负载指标。大 GeoJSON 的内存开销仍明显，未在这次 API 重构中重写 GDAL 数据读取路径。

本机原始结果、版本 probe 和复现程序保存在 `/private/tmp/spatial-core-gdal-validation-joy0o9yf/`；不将用户数据或完整原始产物加入仓库。

## 已知边界

- GDAL 3.12.2 的部分 FlatGeobuf 字符串 NULL 读回为空串；Shapefile 默认编码可能丢失中文，需按数据显式指定 `ENCODING=UTF-8`。未把格式转换宣传为任意数据的无损复制。
- 类型化 API 不提供跨进程原子 create-new、通用事务回滚或失败后删除产物；调用方独占目标，检查错误的输出状态后再决定清理/重试。
- 物理别名保护依赖 driver 文件列表和受测伴随规则；未知 driver 隐藏文件及远程/VSI 别名无法完整枚举。
- 未运行 Windows、其他 GDAL 版本的完整套件、远端 CI、真实 S3、生产负载或真实磁盘故障。低版本字段引用按契约拒绝，但不能把版本分支测试等同于实际跨版本验证。
- raster 新 builder 仍属于后续功能，本轮已完成的是共享控制桥接及矢量转换。

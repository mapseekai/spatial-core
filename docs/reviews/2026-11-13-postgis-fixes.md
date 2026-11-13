# PostGIS：8 项问题修复与最终复审

日期：2026-11-13。对象为 spatial-core 当前未提交工作区；Git 基线为 `dab0024d712453dd02fb1aebac80ac765aaa72ab`。本报告承接 [修复前审查](2026-11-13-postgis.md)，原始证据保持不变。

**结论：原有 8 项问题全部修复，复审追加的组合场景也已修复。当前检查范围内没有剩余已确认阻断问题。** 完整本地检查、24 项数据库回归以及 26 组真实导入已在最终实现上重跑。按要求，在首次修复和真实导入后派发两位独立 agent，分别复审数据契约与事务生命周期；两者均对追加修复进行了独立复测。

## 修复与回归对应关系

| 问题 | 最终实现 | 正式回归与独立验证 |
| --- | --- | --- |
| R1：raster catalog 舍入导致正确输入被拒绝 | `raster/table.rs` 区分 catalog 约束存在性与真实 header 精度。所有写入模式都逐行核对 scale、skew、SRID、各波段类型、NoData 和 InDB/OutDB；不舍入源值 | `precise_scale_nodata_and_reprojection_survive_catalog_rounding`；真实 Cloud、城市影像和 Landsat 重投影；ReplaceRows header 回归见下文 |
| R2：SRID 预检与 reader 竞争 blocking pool | SRID 定义通过握手交给已有 source worker 比较；连接获取先于 reader 启动，避免冷池 DNS 与 reader 互等 | `both_writers_finish_with_one_blocking_thread`：冷 localhost 池、单 blocking 线程、100 个点和 64 个 tile；另有预检退出与 SRID 错误单测 |
| R3：trigger 后的实际行数与成功报告不一致 | COPY 完成后执行延迟约束，建索引/约束等 DDL 后再次执行延迟约束，再做精确校验和 ANALYZE；最后核对实际行数并计算最终 extent，不符即回滚 | `after_and_deferred_triggers_are_checked_before_reporting_success`；`ddl_trigger_row_changes_are_checked_after_finalization`；改写几何的 trigger 保持数量时，报告使用最终范围 |
| R4：metadata 混合不同表版本 | vector/raster 均在 repeatable-read 事务的首次快照前取得 ACCESS SHARE 表锁，再读取 catalog、数量及范围，锁持有至事务结束 | `metadata_blocks_replacement_until_its_snapshot_is_complete`；独立反向竞争探针验证等待已进行的 DROP/CREATE 后返回完整的新表版本 |
| R5：CancelRequest 失败后在繁忙连接上无限等待回滚 | 取消投递失败即丢弃连接；取消投递和回滚各限 5 秒；保留原始错误及 `CleanupTimeout`，取消过的连接不回池 | `failed_cancel_connection_discards_without_waiting_for_running_sql`；独立吞掉取消包探针约 5 秒返回 CleanupTimeout、池中连接数为 0 |
| R6：ZIP 明确成员被当作目录 | 先尝试 GDAL 直接打开，失败后仅对 ZIP 做目录回退；回退失败保留原打开错误 | `zip_files_and_archive_roots_open_without_treating_members_as_directories` 覆盖根、嵌套目录、Shapefile/GeoJSON 明确成员和不存在成员；真实 City.zip 根及成员均导入 385 行 |
| R7：字符串到浮点静默舍入 | 按目标 f32/f64 的精确二进制值展开为完整十进制，与输入数学值比较；显式非有限值和数值溢出分开处理，NUMERIC 保持精确十进制路线 | `textual_floats_require_exact_binary_values`；`lossy_text_float_conversion_rolls_back_existing_rows`；独立 Decimal 判定与数据库读回的 180 组输入全部通过 |
| R8：混合 SRID 的 raster extent 无定义 | metadata 只有单一实际 SRID 时才返回 extent；异构边缘 tile 尺寸本身不阻止返回范围 | `mixed_srids_have_no_extent_but_edge_tiles_do` 覆盖 4326/3857 混合、NULL、同 SRID 边缘 tile 和全 NULL |

回归均放在对应模块内，没有将临时消费者整套搬入默认 CI。数据库测试使用随机 schema；源 fixture、故障代理、权限及重跑要求见 [TESTING](../TESTING.md)。

## 两位 agent 复审及追加修复

数据契约 agent 复审 raster 精度、ZIP、浮点、异构范围及报告语义，发现 `ReplaceRows` 原本绕过新表 finalization 中的精确 header 校验。现将每次写入的 `verify` 与仅新表执行的 DDL 分开；新增 `replace_rows_checks_precise_headers_after_all_triggers`，覆盖普通/延迟 trigger 对 NoData 的微小修改，以及手工无约束表的 SRID、scale、skew、pixel type 和波段数变化。回归验证拒绝修改并保留旧行；agent 独立运行通过。

事务 agent 复审连接、取消、trigger、metadata 和提交边界，发现并复测以下修复：

- 冷 localhost 池仍可能等待被 reader 占住的唯一 blocking 线程：先获取连接后启动 reader；独立公开 API 探针约 189 ms 完成。
- DDL event trigger 可能在早期行数检查后插入记录：最终行数检查移至全部收尾 SQL 之后；独立探针返回 SchemaMismatch，事务完整回滚。
- 尚在 blocking 队列中的 reader 取消会等待无关任务：使用原子启动状态撤销尚未开始的 reader，不等待队列中的 JoinHandle；已经开始的 reader 仍协作取消并等待收尾。新增 `queued_sources_cancel_without_waiting_for_unrelated_blocking_work` 覆盖显式 stop 与 drop，独立公开 API 探针在无关 blocker 释放前返回 Cancelled。

事务 agent 还独立重跑 8 项事务数据库回归，并验证 COMMIT 实际成功后丢弃响应时返回 `CommitOutcomeUnknown`、连接被丢弃且新数据确实已经提交。数据 agent 独立完成 180 组文本浮点判定及读回。两者最终均报告其检查范围内无剩余已确认阻断问题；并未声称覆盖所有平台、TLS 或生产压力。

报告语义已明确：trigger 可在目标约束允许范围内改写属性/几何；成功报告的数量与 extent 描述最终存储结果。`RasterWriteReport` 的 origin、source/output size 描述 GDAL 输入/输出计划，不承诺根据 trigger 重建像元网格；scale、skew、SRID 和波段契约会逐行核对。关闭连接不等于确认服务器立即停止，原生 GDAL I/O 也不受数据库清理的 5 秒期限约束。

## 最终检查

| 检查 | 最终结果 |
| --- | --- |
| `bash scripts/ci/check.sh` | 结构检查、rustfmt、全部 targets 的 Clippy `-D warnings`、37 项默认测试、2 项 Rustdoc 测试、Rustdoc 构建、转换示例及 diff 空白检查全部通过 |
| `bash scripts/ci/test-postgis.sh` | 设置专用 DSN 与挂载目录 TMPDIR 后，24 项真实数据库回归全部通过；未将默认 ignored 项计为通过 |
| 独立真实数据消费者 | 最终实现上 26 组：23 组成功，3 组按契约拒绝，0 组意外失败 |
| agent 独立复测 | 数据侧 180 组浮点与精确 header；事务侧 8 项正式回归及冷连接、排队取消、DDL、metadata、取消包丢失和 COMMIT 响应丢失探针通过 |

本次增加 5 项默认测试、9 项数据库回归，扩展既有提交边界测试。首次最终检查曾发现嵌套 ZIP 测试的 unsafe 注释未紧贴代码，已修正；首次数据库重跑遗漏 TMPDIR，导致容器无法读取主机临时目录中的两组 OutDB fixture，按文档补齐同路径只读挂载环境后全部通过。失败日志一并保留，未将其计为成功，也未修改生产代码绕过测试环境要求。

本轮客户端：macOS arm64、Rust 1.98.1、GDAL 3.12.2。数据库使用机器上已有镜像新建的专用可丢弃容器：PostgreSQL 17.7、PostGIS/postgis_raster 3.6.1、GDAL 3.10.3、PROJ 9.6.0、GEOS 3.13.1，Linux amd64。原有容器与业务库未修改。没有为修复新增 crate；仅补齐 Tokio 功能用于清理期限和测试代理。

## 真实数据导入结果

| 数据/模式 | 结果 |
| --- | --- |
| point、line GeoJSON | 8 / 74 feature；数量、几何及属性核对通过 |
| 3857 Shapefile、独立 3857→4326 重投影 | 各 385 feature；投影几何由 PostGIS 独立比较 |
| world GeoParquet | 177 feature、168 个属性字段；读回抽查通过 |
| 单要素 GeoPackage | 1 feature，通过 |
| City.zip 根与明确 Shapefile 成员 | 各 385 feature，均通过 |
| 建筑 GeoPackage / FlatGeobuf | 各完整导入 251,014 个 MultiPolygonZ feature |
| 完整 Landsat 4 / 5 | 3944×3524 / 3911×3478，各 224 tiles |
| 高景 UInt16、COG UInt16 | 原分辨率裁块，分别 20 / 12 tiles |
| Cloud、城市 RGB、吉林 RGB | 原分辨率裁块，分别 20 / 20 / 6 tiles；此前精细 scale 失败已消除 |
| 高景 overlap、Whole、OutDB Tiled | 20 / 1 / 20 tiles；实际像元核对通过 |
| 吉林 Whole、Landsat OutDB Whole | 各 1 tile，通过 |
| Landsat→3857 | 516×285、6 tiles；独立 gdalwarp VRT 核对输出网格及像元 |
| 3857.parquet | 缺失可用 CRS，明确拒绝，目标不存在 |
| unknown_geojson / unknown_fgb | 几何 family 未提供显式支持 schema，明确拒绝，目标不存在 |

成功用例累计导入 **503,828 个 feature、575 个 raster tile**，含同源不同格式和不同模式的重复导入。核对全部成功表数量，抽查 46 个几何、905 个属性值和 417 个跨波段像元；核对全部成功 tile 的尺寸、原点、scale、skew、SRID 与波段数。41 个未重投影几何按全维度 EWKB 精确比较；5 个投影样本由独立 PostGIS 投影比较，最大差约 1.42e-14 度。像元为抽样检查，不能声称逐像元或逐属性完全比对。

原始桌面输入未改动。大型遥感输入采用记录了窗口的原分辨率裁块，完整 Landsat 和建筑数据另行全量导入；裁块不是整幅大影像的负载测试。OutDB 仅通过临时目录的同路径只读挂载完成本地容器验证。

## 证据与使用边界

[归档目录](../../artifacts/reviews/2026-11-13-postgis-fixes/README.md) 包含最终源码哈希、正式检查日志、26 组参数/结果、裁块窗口和两位 agent 的复现程序及日志。旧审查证据未覆盖。文档已同步更新 README、POSTGIS_REFACTOR、TESTING 和 POSTGIS_VALIDATION。

验收结束后已删除本任务的专用容器及匿名数据卷、临时裁块、独立构建目录和连接凭据。用户原有容器与桌面原始数据未修改。

本轮未重跑 TLS、真实 S3、远程 OutDB、远端 CI、Rust Linux/Windows 客户端或持续压力；没有吞吐提升、峰值 RSS 或所有 GDAL 驱动的结论。文本 `0.1` 写入 real/double 会按无损契约拒绝，精确十进制应显式选择 NUMERIC。COMMIT 响应丢失仍需调用方核对后决定重试，不能自动当成已回滚。外部消费者迁移、提交、推送和发布不在本次操作范围内。

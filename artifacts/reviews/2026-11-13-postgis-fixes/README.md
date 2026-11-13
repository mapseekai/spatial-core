# 2026-11-13 PostGIS 修复验收证据

最终结论与逐项修复见 [完整报告](../../../docs/reviews/2026-11-13-postgis-fixes.md)。旧审查证据保存在相邻 `2026-11-13-postgis` 目录，未覆盖原始日志或消费者。

| 文件/目录 | 内容 |
| --- | --- |
| `reviewed_sources.sha256` | 最终源码、测试、清单及本轮文档哈希 |
| `check_final.log` | 完整本地检查：37 项默认测试、2 项 Rustdoc 测试及其余检查通过 |
| `database_final.log` | 专用 Docker PostGIS 上 24 项数据库回归全部通过 |
| `check_final_first_attempt.log` | unsafe 注释位置修正前的 Clippy 失败，保留执行经过 |
| `database_final_missing_mount_env.log` | 漏设 TMPDIR 时容器无法读取两组 OutDB fixture 的失败，保留环境问题 |
| `environment_versions.log` | 实际数据库服务端版本 |
| `real_cases.json` / `real_results.json` | 最终 26 组参数与原始结果：23 成功、3 明确拒绝、0 意外失败 |
| `real_results_before_rereview.json` | 追加组合场景修复前的首次 26 组重跑结果 |
| `crop_sources.json` | 原始桌面影像路径、x/y/width/height 原分辨率裁块窗口 |
| `real_imports/` | 实际导入和数量、EWKB、属性、tile header、像元核对消费者 |
| `data_rereview/` | 数据 agent 的 ReplaceRows NoData、报告 origin、180 组浮点探针与日志 |
| `final_tx_probe/` | 事务 agent 的冷连接、DDL、metadata、排队 reader、取消包和 COMMIT 响应丢失探针与日志 |
| `probe_archive_check.log` | 归档后两份复审消费者的 locked/offline 编译检查 |

归档探针只调整了本库依赖与仓库 fixture 的相对路径、浮点临时 fixture 的环境变量路径，并运行 rustfmt；复现逻辑保持不变。`data_rereview/float_cases.tsv` 固定了独立 Decimal 判定的 180 个输入及接受/拒绝预期；消费者另检查成功写入后的浮点位值。各日志中的旧临时路径是执行时位置。

## 复跑

从仓库根执行。需要 Rust 1.98.1、GDAL 及开发库，以及启用 postgis/postgis_raster 的**专用可丢弃数据库**。将连接串放在环境变量 `SC_REVIEW_DSN`，不要写入仓库。探针需要 TCP 127.0.0.1、NoTls、密码连接以及测试用户的超级用户权限；会创建或替换 `review_main`、`review_final_data`、`review_final_tx` 的测试对象，并注入 DDL/连接故障，不得对业务库运行。

`SC_REVIEW_FIXTURES` 指向本任务临时可写目录，数据库需要通过同一绝对路径只读访问 OutDB fixture。不要改真实输入文件权限来满足挂载。

正式回归：

```sh
export SPATIAL_CORE_TEST_POSTGRES_DSN="$SC_REVIEW_DSN"
TMPDIR="$SC_REVIEW_FIXTURES" bash scripts/ci/test-postgis.sh
bash scripts/ci/check.sh
```

复审消费者示例：

```sh
cargo +1.98.1 run --locked --manifest-path artifacts/reviews/2026-11-13-postgis-fixes/data_rereview/Cargo.toml --bin spatial-core-data-rereview
cargo +1.98.1 run --locked --manifest-path artifacts/reviews/2026-11-13-postgis-fixes/data_rereview/Cargo.toml --bin float_cases
cargo +1.98.1 run --locked --manifest-path artifacts/reviews/2026-11-13-postgis-fixes/final_tx_probe/Cargo.toml -- hostname
cargo +1.98.1 run --locked --manifest-path artifacts/reviews/2026-11-13-postgis-fixes/final_tx_probe/Cargo.toml -- queued_source
cargo +1.98.1 run --locked --manifest-path artifacts/reviews/2026-11-13-postgis-fixes/final_tx_probe/Cargo.toml -- event_trigger
cargo +1.98.1 run --locked --manifest-path artifacts/reviews/2026-11-13-postgis-fixes/final_tx_probe/Cargo.toml -- metadata_wait
cargo +1.98.1 run --locked --manifest-path artifacts/reviews/2026-11-13-postgis-fixes/final_tx_probe/Cargo.toml -- rollback_timeout
cargo +1.98.1 run --locked --manifest-path artifacts/reviews/2026-11-13-postgis-fixes/final_tx_probe/Cargo.toml -- commit_reply_loss
```

`tokio_queued_abort` 是 Tokio 排队任务行为的诊断对照，其 700 ms 超时本身是预期观察，不是库测试失败。部分探针打印 Err 后正常退出，因此不能只依据退出码判断结果；正式回归用例有对应断言。

真实数据程序参数为 `vector|raster id path [key=value...]`，使用 ReplaceTable 写入 `review_main`：

```sh
psql "$SC_REVIEW_DSN" -v ON_ERROR_STOP=1 -c 'CREATE SCHEMA IF NOT EXISTS review_main;'
cargo +1.98.1 run --locked --manifest-path artifacts/reviews/2026-11-13-postgis-fixes/real_imports/Cargo.toml -- vector point /Users/zhang/Desktop/测试数据/point.geojson
```

其余参数见 `real_cases.json`。重建裁块时，以 `crop_sources.json` 的 source/window 执行 `gdal_translate -srcwin x y width height source destination`，输出至当前临时 fixture 目录，并替换清单中已经失效的临时绝对路径。Landsat 重投影 oracle 由 `gdalwarp -of VRT -t_srs EPSG:3857 -r near -wm 64 landsat_crop.tif landsat_3857.vrt` 独立生成；通过 `oracle=` 提供给核对程序，库仍读取原裁块并自行重投影。

核对程序只有 `PASS|...` 加退出码 0 才表示通过；`WRITE_ERROR` 也可能退出 0。本轮仅 `parquet3857` 的缺失 CRS 和 `unknown_geojson` / `unknown_fgb` 的未指定几何 schema 计为预期拒绝，并逐项确认目标表不存在。所有其他用例要求成功和读回断言通过。

没有归档密码、DSN、原始桌面数据、影像裁块、数据库文件、容器卷或编译产物。消费者独立锁文件不代表主库依赖升级。耗时用于复现等待和取消行为，不是吞吐基准。

验收完成后，本任务的专用容器、匿名数据卷、临时目录与凭据已清理；复跑需要重新建立独立测试环境。

# 2026-11-13 PostGIS 审查证据

结论与修复建议见 [完整报告](../../../docs/reviews/2026-11-13-postgis.md)。本目录包含三份独立消费者、原始输出和真实数据案例清单，不改变库源码或默认 CI 测试集合。

| 文件/目录 | 内容 |
| --- | --- |
| `reviewed_sources.sha256` | 被审查的 postgis 源码、关联 GDAL/tiling 文件及 Cargo 清单哈希 |
| `real_cases.json` | 最初 26 组导入参数 |
| `real_results.json` | 最终核对结果；ZIP 根参考源与重投影比较修正后的结果 |
| `real_results_initial.json` | 初始原始输出，保留测试端误报经过 |
| `crop_sources.json` | 原始 TIFF 路径与 x/y/width/height 裁块窗口 |
| `database_regression.log` | 本轮 Docker 数据库上 15 项现有集成测试的完整输出 |
| `real_imports/` | 真实数据导入与行数、EWKB、属性、tile、像元核对程序 |
| `data_contracts/` | catalog 精度、ZIP 成员、异构范围、文本浮点、OutDB 对照的诊断程序 |
| `transactions/` | trigger、metadata DDL 竞态、blocking pool、取消通道故障的诊断程序及各项日志 |

原始日志中的 `/private/tmp/sc_review_8hffshn0` 是当时的临时目录，测试后已清理。归档消费者仅调整了 crate 相对路径、事务消费者的统一 DSN 环境变量和临时 fixture 路径，并经 rustfmt 与 locked/offline 编译检查；没有改变复现逻辑。时间数字用于说明观察到的等待，不是性能基准。

## 复跑条件

从仓库根目录执行。需要 Rust 1.98.1、GDAL 及对应本地开发库、已安装 postgis/postgis_raster 的**专用可丢弃测试数据库**。这些程序会替换 `review_main`、`review_data`、`review_transactions` 中的同名测试对象；不要传入业务数据库。取消与 metadata 的代理用例要求数据库监听 127.0.0.1 的 TCP 端口，并使用密码连接和 NoTls。

从本地环境提供 `SC_REVIEW_DSN`，不要写入仓库文件；`SC_REVIEW_FIXTURES` 指向本任务的临时可写目录。OutDB 测试要求数据库进程也能通过该绝对路径只读访问 fixture 文件。本轮将同一路径只读挂载进临时容器，没有改变原有实例配置。

```sh
psql "$SC_REVIEW_DSN" -v ON_ERROR_STOP=1 -c 'CREATE SCHEMA IF NOT EXISTS review_main; CREATE SCHEMA IF NOT EXISTS review_data; CREATE SCHEMA IF NOT EXISTS review_transactions;'
cargo +1.98.1 run --locked --manifest-path artifacts/reviews/2026-11-13-postgis/transactions/Cargo.toml -- starvation
cargo +1.98.1 run --locked --manifest-path artifacts/reviews/2026-11-13-postgis/transactions/Cargo.toml -- starvation_control
cargo +1.98.1 run --locked --manifest-path artifacts/reviews/2026-11-13-postgis/transactions/Cargo.toml -- triggers
cargo +1.98.1 run --locked --manifest-path artifacts/reviews/2026-11-13-postgis/transactions/Cargo.toml -- metadata
cargo +1.98.1 run --locked --manifest-path artifacts/reviews/2026-11-13-postgis/transactions/Cargo.toml -- cancel
```

上述消费者会清理对应事务测试表。死锁用例在 3 秒后取消，正常对照为两个 blocking 线程。取消故障用例使用三次 pg_sleep(2)，应按日志中的“取消请求失败后仍等待”判断，不以恰好 6037ms 为断言。

真实数据程序参数为 `vector|raster id path [key=value...]`，通过环境变量连接数据库。测试端使用 ReplaceTable，每次覆盖对应测试表。

```sh
cargo +1.98.1 run --locked --manifest-path artifacts/reviews/2026-11-13-postgis/real_imports/Cargo.toml -- vector point /Users/zhang/Desktop/测试数据/point.geojson
cargo +1.98.1 run --locked --manifest-path artifacts/reviews/2026-11-13-postgis/real_imports/Cargo.toml -- vector zip_city /vsizip//Users/zhang/Desktop/测试数据/City.zip oracle=/vsizip//Users/zhang/Desktop/测试数据/City.zip/City/CN_city.shp
cargo +1.98.1 run --locked --manifest-path artifacts/reviews/2026-11-13-postgis/real_imports/Cargo.toml -- raster rgb16 "$SC_REVIEW_FIXTURES/rgb16.tif"
```

其他参数见最终 `real_results.json` 的 flags。重建真实裁块时，将 `crop_sources.json` 各项的 source 和 window 用于 `gdal_translate -srcwin x y width height source destination`，目标写入当前临时 fixture 目录，不使用已经清理的旧路径。不需要复制或修改原始桌面文件。

`data_contracts` 程序自行生成三种 2×2 Float64 TIFF；以下附加 fixture 由运行前准备：

- `review_data_cloud64.tif`：真实 Cloud.tiff 的 64×64 原分辨率裁块，任意有效 srcwin，保留原仿射变换。
- `review_data_points.zip`：根目录包含仓库 `testdata/points.geojson`，成员名为 `points.geojson`。
- `review_data_shp.zip`：用 ogr2ogr 将同一 points.geojson 转为 Shapefile，ZIP 根包含 `points.shp` 及 sidecar。
- `review_data_float_text.geojson`：两个 EPSG:4326 点，属性 value 分别是字符串 `9007199254740993`、`1.0000000000000001`。必须保持字符串，不能先由 JSON 数字解析舍入。

```sh
cargo +1.98.1 run --locked --manifest-path artifacts/reviews/2026-11-13-postgis/data_contracts/Cargo.toml
```

这三份程序是审查诊断消费者，部分错误路径会打印 `WRITE_ERROR`/`Err(...)` 后正常退出。因此**不能把退出码 0 当作导入通过**：本轮依据实际输出、读回断言和错误分类，才得到 19 通过、3 明确拒绝、4 缺陷失败。修复时应将报告中的精简复现转为带断言的正式回归，无需把整套临时诊断工具纳入默认 CI。

## 保存范围

没有保存数据库密码、DSN、原始桌面数据、影像裁块、测试数据库目录、容器数据卷或编译产物。固定了当时的源码哈希与依赖锁文件；后续源码变化后应重新运行，旧日志不能证明修复后的行为。

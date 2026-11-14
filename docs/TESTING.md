# 独立验证

## 默认检查

```sh
bash scripts/ci/check.sh
```

要求 Rust 1.98.1（rustfmt、Clippy）、Python 3.11+、GDAL 与 libclang。默认测试使用内存或临时目录生成数据；`testdata/points.geojson` 是三个人工点，不含用户文件。示例临时文件会自动删除。

`.github/workflows/ci.yml` 定义 Ubuntu 24.04 的 GDAL/PostGIS 全套检查和 macOS 默认检查。远端检查须以 [GitHub Actions](https://github.com/mapseekai/spatial-core/actions) 中对应提交的实际结果为准，不能仅凭工作流定义声称通过。Linux 使用系统 GDAL 包；macOS 通过固定版本的 micromamba 和固定提交的 setup-micromamba action，从 conda-forge 安装 `libgdal-core==3.12.2`，不使用 Homebrew 滚动更新的 GDAL。GDAL 绑定 0.19 要求 3.4+，但不保证兼容后续所有版本；3.13.3 已出现不兼容的 C API 变动。

### macOS 原生依赖隔离

GDAL 3.12.2 的 headers、动态库、`gdal-config` 和 pkg-config 元数据必须来自同一个安装 prefix，PROJ 数据也必须来自该环境。不能只设置 `GDAL_VERSION` 来伪装版本，或将旧 headers 与新库混用。CI 在编译前执行 `scripts/ci/check-gdal.sh`，检查版本和路径，并编译运行 C probe，核对 header、运行库版本及实际载入的动态库路径。

本地可使用 [官方 micromamba](https://mamba.readthedocs.io/en/latest/installation/micromamba-installation.html) 创建独立环境（以下命令在 Bash 中执行）：

```sh
brew install llvm pkg-config
micromamba create --yes --name spatial-core-ci --override-channels --channel conda-forge \
  --strict-channel-priority 'libgdal-core==3.12.2'
eval "$(micromamba shell hook --shell bash)"
micromamba activate spatial-core-ci
export GDAL_HOME="$CONDA_PREFIX"
export GDAL_INCLUDE_DIR="$GDAL_HOME/include"
export GDAL_LIB_DIR="$GDAL_HOME/lib"
export GDAL_DATA="$GDAL_HOME/share/gdal"
export PROJ_DATA="$GDAL_HOME/share/proj"
export PROJ_NETWORK=OFF
export PKG_CONFIG_PATH="$GDAL_HOME/lib/pkgconfig"
export LIBCLANG_PATH="$(brew --prefix llvm)/lib"
export RUSTFLAGS="${RUSTFLAGS:-} -C link-arg=-Wl,-rpath,$GDAL_LIB_DIR"
export RUSTDOCFLAGS="${RUSTDOCFLAGS:-} -C link-arg=-Wl,-rpath,$GDAL_LIB_DIR"
bash scripts/ci/check-gdal.sh "$GDAL_HOME" 3.12.2
bash scripts/ci/check.sh
```

Conda 动态库使用 `@rpath`，因此除了 Rust 编译时的库搜索路径，还需上述运行时 rpath；`RUSTDOCFLAGS` 同样配置，避免 doctest 生成的独立程序找不到 GDAL。该配置只影响当前测试进程，不重写 Homebrew symlink 或生产软件。macOS 仍执行完整 `check.sh`；Linux 的默认检查、全部数据库回归和数据库示例保持不变。

GeoParquet 需要 GDAL Parquet driver。当前默认回归不会证明这个可选原生驱动可用，也不覆盖真实 S3、服务端算子/Temporal、Windows 或生产负载。本次独立 TLS 实测单列于验证记录，不是默认 CI 的一部分。

## GDAL 矢量转换回归

默认检查还运行 `vector_convert` 示例及矢量转换回归：GPKG、GeoJSON、FlatGeobuf、Shapefile 的真实写入和读回；四种写入模式；schema/CRS 追加预检；字段引用、SQL、过滤和驱动选项；多文件/路径别名保护；空表、NULL、整数、Z/M/ZM、混合几何、显式修复和跳过失败；取消/panic、诊断隔离、原生工作线程回调以及关闭错误组合。

本 API 要求构建及运行 GDAL >= 3.7；复杂字段引用需 3.9+。可选 GeoParquet 测试单独执行，缺驱动必须失败，不能记为通过：

```sh
cargo test --locked --lib vector::translate::tests::optional_geoparquet_roundtrip -- --ignored
```

`test-postgis.sh` 只选择 `postgis::` 下的 ignored 数据库测试，不会误运行可选 Parquet 项。PGDUMP 默认回归仅检查生成 SQL，不执行 SQL。

桌面或其他真实数据可用 `cargo run --locked --example vector_convert -- INPUT NEW_OUTPUT DRIVER [LAYER]` 转换并重新打开核对要素数。输出须使用新临时目录；大文件耗时、内存观察、字段/CRS 抽检另记，不把要素数相同等同于无损。GDAL 3.12.2 实测 FlatGeobuf 字符串 NULL 在读回时可能为空串，Shapefile 的字段名/类型也存在格式限制；这些限制由回归明确区分。

关闭错误组合通过最小私有 `record_close` 故障接缝验证，不模拟整个 GDAL。它证明主错误及两个关闭错误不会互相覆盖；另用真实 NativeBuildVrt 向目录路径写入，覆盖延迟写入时 GDALClose 返回成功但 CPL 报失败的路径，核对 `Close / CloseOutput`、诊断与清理错误。该回归分别使用空 Control 和仅取消 token；不等于对所有驱动或真实磁盘耗尽做了故障注入。

## 真实数据库回归

只使用新建的、可丢弃的数据库。测试用户需创建 schema、表、索引与序列的权限；用数据库管理者提前创建 `postgis` 和 `postgis_raster` 扩展。测试会创建随机名称的对象，并主动测试替表、取消和失败回滚，不能指向业务库。

```sh
psql "$SPATIAL_CORE_TEST_POSTGRES_DSN" -v ON_ERROR_STOP=1 \
  -c 'CREATE EXTENSION IF NOT EXISTS postgis;' \
  -c 'CREATE EXTENSION IF NOT EXISTS postgis_raster;'
bash scripts/ci/test-postgis.sh
```

脚本要求 DSN 并运行全部 ignored database tests，测试数量由源码决定。未配置 DSN 或缺扩展必须失败。默认 `cargo test` 的 ignored 项不属于数据库验证成功。测试使用随机 schema 和合成 fixture，并在正常退出时删除。

主要覆盖：

- vector：bigint identity、精确 decimal、微秒/时区、NULL/空数组、文本转义、字段改名与大小写、空表/Z/M/ZM/EMPTY、坐标数值重投影、类型恢复失败回滚。
- source：GeoPackage + SQLite JSON 表达式在首次/中途读取时触发原生 GDAL 错误，验证失败不能变成 EOF 或空源。
- raster：7 种像元类型、NaN/缺失 NoData、多波段、skew、边缘窗口；InDB/OutDB × Whole/Tiled 的真实像元读回、catalog 约束与异构 metadata。
- transaction：并发 writer 按目标串行化、不同表可继续写入、锁超时、pool max_size=1、取消与回滚、保留既有约束、继承/分区/RLS 拒绝；COMMIT 边界取消、延迟约束拒绝与终止测试 backend 后的未知结果。
- connection：事务内扩展 schema 解析、连接设置恢复、取消后连接丢弃。
- 2026-11-13 修复回归：精细分辨率/NoData 和 raster 重投影；vector/raster 在单 blocking 线程和冷 localhost 池下完成，排队 reader 取消不等待无关 blocking 任务；普通、延迟及 DDL event trigger 改变行数时回滚、改写几何时报告最终范围；ReplaceRows 逐行精确检查 raster header，包括手工无约束表；metadata 与外部 DROP/CREATE 竞争；取消连接单独失败时及时丢弃连接；ZIP 根、嵌套目录及明确成员；文本浮点精度拒绝与旧行保留；混合 SRID 与边缘 tile 的范围区别。

协议故障测试使用本机临时 TCP 代理，专用 DSN 必须指定 TCP host（可用 127.0.0.1）；测试会暂停指定 SQL 或拒绝新增取消连接，只转发到该专用库。它们不依赖真实网络故障或固定睡眠猜测执行阶段。

DDL event trigger 回归需要 PostgreSQL 超级用户权限，只匹配本次随机 schema 的目标表，结束后删除。

数据库测试用户还需要设置测试会话的 `postgis.enable_outdb_rasters` / `postgis.gdal_enabled_drivers`，以及终止同一测试用户所创建 backend 的权限。OutDB 测试要求 PostgreSQL 能从**相同绝对路径**读取合成 TIFF；仅能连接远端数据库不够。CI 将 `/tmp/spatial-core-fixtures` 只读挂入服务容器，数据库步骤把 `TMPDIR` 指向该目录；仅合成 raster fixture 设置为 0644。不会为了让测试通过而修改真实输入文件权限。

公开示例也使用同一个专用 DSN，创建独立随机 schema 并在结束后清理：

```sh
cargo run --locked --example postgis_vector
cargo run --locked --example postgis_raster
```

TLS 不作为所有测试的隐式要求。需要额外验证时，使用调用方的 TLS crate 构建一个独立消费者，将**同一个 connector** 传给 deadpool Manager 和 `PostGisClient::from_pool`。以 `sslmode=require`、可信 CA 和正确主机名连接，查询 `pg_stat_ssl` 确认加密；在观察到 writer 等锁后取消，断言回滚、旧数据及新 backend PID。不得以关闭证书校验代替此验证。本次使用 `postgres-native-tls 0.5.3` + `native-tls 0.2.18` 和临时 CA，未向库添加 TLS 实现依赖。

## 复现与依赖

`Cargo.lock` 从 MapSeek 已锁版本裁剪，不依赖相邻源码或源工作区的 feature 合并。使用 `--locked`；离线环境可再加 `--offline`，前提是依赖已经在 Cargo cache。首次安装 native 工具或下载依赖需按运行环境权限处理。

macOS 的 Homebrew GDAL 和 PostGIS 必须链接相容的 native 库。若安装了多个 GDAL keg，不要为测试擅自重写全局 symlink；使用独立测试进程或容器的匹配环境。测试数据库和目标目录应独立，不复用用户生产数据库。

重构后的实测详情见 [GDAL 验证记录](GDAL_VALIDATION.md) 和 [PostGIS 验证记录](POSTGIS_VALIDATION.md)。[提取基线验证记录](VALIDATION.md) 保留当时事实，不作为当前 API 的测试结果。

## GDAL 栅格与矢量处理

新入口见 [处理设计](GDAL_PROCESSING_DESIGN.md)。默认测试逐项创建并读回真实 GDAL 数据，覆盖窗口/像元类型/NoData、重投影拼接、VRT 依赖、坡度与颜色表、属性栅格化、连通分类矢量化、三维等值线、填洞和筛选，以及裁剪/简化/加密/SQL buffer。

共用执行路径还测试预取消、各算法回调取消和 panic、原生参数边界、输出及伴随文件冲突、别名、CPL 状态恢复和来源文件保留。`check.sh` 执行四个自包含转换/处理示例。真实桌面数据验证记录见 [本轮验证](GDAL_PROCESSING_VALIDATION.md)，不把默认忽略的数据库或可选 Parquet 测试计为通过。

审查回归另外覆盖：具体几何类型的 Z/M/ZM 保留、Shapefile 字段缩名、Int8 分类、多波段联合 NoData 与显式 UInt8 mask、混合波段类型复制拒绝、FillNodata 高精度保护、大小写驱动名和辅助输入别名、COG/FillNodata 临时文件隔离。Fill/Sieve 取消和 panic 除复制阶段外，还单独覆盖复制完成后的原生算法阶段。


2026-11-14 对抗审查修复回归还覆盖：BuildVrt typed/native 拒绝不能持久化的源打开选项，原输出保持不变，Translate VRT 代理路径关闭重开后像元一致；驱动目标后补时仍识别已登记源的别名；目录包含、软链接、硬链接及伴随文件保护；多输入打开之间的确定性取消；七类波段入口及三类显式掩膜的零、越界、整数溢出错误契约；空 Control 不安装原生回调；低层 Translate/Warp 输出关闭不释放借用输入。性能探针使用 release 构建、交替顺序和相同读回校验，记录中位数，不在 CI 中设置受机器负载影响的耗时断言。

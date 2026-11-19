# DuckDB 矢量分析

`duckdb` 是可选 feature，默认构建不会链接 DuckDB。当前提供 [设计](DUCKDB_DESIGN.md) 的矢量读取闭环：独立 Session、本地/HTTP/S3 输入、metadata 与临时 view、参数化 SQL、Arrow 批次、本地 GeoParquet 输出。空间函数直接使用 SQL；类型化六类算子、PostGIS 流式桥接和性能调优仍是后续阶段。

`Reader::Gdal` 使用本仓库修补并签名的 Spatial 扩展。补丁把 GDAL 的错误码及 CPL failure 传回 Rust，包括已经读出部分记录后失败的情况；`ST_Read_Meta` 和 `ST_Read` 共用 driver/open options/sibling 配置。构建还修正 GDAL 的 FlatGeobuf Arrow NULL 几何标记，保留 NULL 与 EMPTY 的区别，并支持 Cargo 默认 Debug 引擎的跨模块分配计数。未经修补的官方扩展可用于 Release 引擎的 GeoParquet 和空间 SQL，但它的 GDAL reader 会在 I/O 前明确拒绝。

## 构建与部署

```toml
spatial-core = { path = "../spatial-core", features = ["duckdb"] }
```

锁定 Rust binding `duckdb = 1.20000.2`，bundled 引擎为 **v2.0.0**。核心、Parquet、JSON 从依赖源码编译。多格式读取还需要构建 patched Spatial；版本、平台和签名均由 DuckDB 校验。源码版本、补丁和许可证见 [原生扩展](../third_party/duckdb-spatial/README.md)。

构建机需要 Git、Python 3、C/C++ 编译器、CMake、Ninja、OpenSSL、pkg-config，以及 vcpkg 对应的平台工具。脚本支持 macOS arm64 与 Linux x86_64；需要联网下载固定版本源码。使用独立构建目录和新的部署目录：

```sh
# 开发/CI 可创建专用临时签名密钥；生产使用受控构建系统保管的密钥。
umask 077
openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out /private/build/spatial-private.pem
bash scripts/native/build-duckdb-spatial.sh \
  /private/build/native /your/deployment/duckdb-extensions /private/build/spatial-private.pem

# 与本机平台匹配：Linux x86_64 为 linux_amd64，Apple Silicon 为 osx_arm64。
platform=osx_arm64
curl --fail --location --retry 2 --max-time 180 \
  "https://extensions.duckdb.org/v2.0.0/${platform}/httpfs.duckdb_extension.gz" \
  -o /your/deployment/duckdb-extensions/httpfs.duckdb_extension.gz
gzip -d /your/deployment/duckdb-extensions/httpfs.duckdb_extension.gz

SPATIAL_CORE_DUCKDB_EXTENSION_DIR=/your/deployment/duckdb-extensions \
SPATIAL_CORE_DUCKDB_SPATIAL_PUBLIC_KEY=/your/trusted/spatial-public.pem \
  cargo run --locked --features duckdb --example duckdb_install
```

部署目录包括 `v2.0.0/<platform>/spatial.duckdb_extension`、`spatial-public.pem`、`spatial-build.json` 和官方签名的 `httpfs.duckdb_extension`。私钥不进入部署目录。应用必须从可信配置显式提供公钥，不能把一个来源未知的扩展及其同目录公钥一起视为可信。生产打包保留构件哈希和第三方许可。

部署时在最终路径运行 `duckdb_install`，只从本地仓库 `INSTALL` 到同目录的 `v2.0.0/<platform>/repositories/spatial_core/`，然后验签加载并检查补丁契约。安装缓存记录仓库的绝对路径；更换路径时，从源构件重新安装。此后保持整个部署目录不可变，升级使用新目录并重启进程。DuckDB 不卸载动态库，稳定安装路径可避免每个 Session 创建一份新的原生库映射。

`Session::open` 只在私有临时目录绑定显式公钥，并从稳定安装路径 `LOAD ... FROM spatial_core`；不会安装扩展、联网获取公钥或写用户默认扩展目录。关闭 Session 清理凭据与临时目录，保留部署构件。`allow_unsigned_extensions` 保持 false；官方 httpfs 仍由 DuckDB 官方公钥验证。[DuckDB 2.0 自定义扩展仓库](https://duckdb.org/2026/10/21/announcing-duckdb-200)

`spatial_public_key = None` 保留 Release 引擎的官方扩展模式，要求部署目录根部的 `spatial.duckdb_extension` 与 `httpfs.duckdb_extension`；`capabilities().gdal_read_supported` 为 false。有公钥时必须检测到 GDAL 读取契约，否则打开失败。`capabilities()` 同时报告引擎/扩展版本及内置 GDAL 驱动，它们与系统 GDAL 是两套能力集合。

Cargo 默认 Debug 构建需要带 `spatial_core_allocator_contract() = 1` 的新签名构件。旧补丁构件和官方 Spatial 缺少此契约，Session 会返回 `Unsupported`，避免空间聚合/连接在释放内存时触发原生断言。检查读取实际链接引擎的分配器状态，不依赖 Rust 的 `debug_assertions` 或调用方的 Cargo profile。升级须重新构建并部署扩展；仅重编译 Rust 不足以修复旧构件。不需要关闭宿主的 Debug 信息或断言。Spatial 扩展自身必须使用脚本的 Release 配置；`DEBUG`（包括 CMake `FORCE_DEBUG`）和 `DUCKDB_DEBUG_ALLOCATION` 在扩展编译时明确拒绝。宿主自定义 `DUCKDB_DEBUG_ALLOCATION` 指针追踪构建同样不在兼容范围，编译时拒绝。

## 完整调用

```rust,no_run
use spatial_core::duckdb::{Control, Reader, Session, SessionConfig, Value, VectorSource};

fn analyze() -> spatial_core::duckdb::Result<()> {
    let mut config = SessionConfig::new("/opt/app/duckdb-extensions", "/var/tmp");
    config.spatial_public_key = Some(std::fs::read_to_string("/opt/app/trusted-spatial-public.pem")?);
    let mut session = Session::open(config)?;
    let control = Control {
        max_rows: Some(1_000_000),
        max_bytes: Some(512 * 1024 * 1024),
        ..Control::default()
    };
    let source = VectorSource::remote("s3://example-bucket/roads.parquet", Reader::GeoParquet);
    session.register("roads", &source, &control)?;
    let report = session.query(
        "SELECT road_id, geom FROM roads WHERE road_class = ?",
        [Value::Text("primary".into())],
    ).write_geoparquet("primary-roads.parquet", &control)?;
    println!("{} rows, {} bytes", report.rows, report.bytes);
    session.close()
}
```

上例的私有 S3 bucket 需要先配置下面的 secret。完全自包含的本地示例：

```sh
SPATIAL_CORE_DUCKDB_EXTENSION_DIR=/your/deployment/duckdb-extensions \
SPATIAL_CORE_DUCKDB_SPATIAL_PUBLIC_KEY=/your/trusted/spatial-public.pem \
  cargo run --locked --features duckdb --example duckdb_vector
```

其他来源只改变 `VectorSource`：

```rust,ignore
VectorSource::local("roads.parquet", Reader::GeoParquet);
VectorSource::local("roads.gpkg", Reader::Gdal).layer("roads");
VectorSource::remote("s3://example-bucket/roads.fgb", Reader::Gdal);
VectorSource::remote("https://example.org/roads.geojson", Reader::Gdal);
VectorSource::remote("https://example.org/roads.parquet?signature=...", Reader::GeoParquet);
```

构造来源不做 I/O；`inspect` 和 `register` 会读取 metadata/绑定 schema。GDAL 的 `allowed_drivers`、`open_options`、`sibling_files` 同时应用到 metadata 和读取。多图层必须显式选择 layer；名称必须精确匹配，不把数字名称当作索引。metadata 不强制扫描整份数据计算数量/范围。`register` 创建惰性临时 view，不覆盖同名关系。路径和别名统一引用，值通过参数绑定；不通过字符串拼接填充 SQL 参数。非 UTF-8 本地路径明确拒绝，不做有损转换。

GDAL 输出按 DuckDB 的标识符规则处理重名。空名称先按绑定层规则填为 `C<从零开始的列索引>`，再按 Arrow 字段顺序保留首个名称，后续冲突追加 `_N` 后缀，比较不区分大小写。使用 `VectorMetadata.geometry_columns` 获取实际几何列名；原始 WKT 和普通属性值均保留。默认 `geom` 别名只用于几何列。原先普通 `wkb_geometry` 属性被错误暴露为 `geom` 的非空间输入，现在恢复使用原属性名。

本期每次登记一个具体文件/数据集，不隐式展开 glob；本地和 S3 路径中的 glob 字符会被拒绝。多个文件可以分别登记，再显式 SQL UNION ALL/JOIN，并自行核对 schema/CRS。HTTP 查询参数原样保留。GeoJSON、FGB、GPKG、Shapefile、KML、GML、CSV 等由内置 GDAL 驱动读取；具体实测格式与协议见 [验证记录](DUCKDB_VALIDATION.md)。Shapefile 等多文件格式需保证旁文件可达，必要时传入 `sibling_files`；目录型/压缩数据集由调用方显式暂存，不隐式下载或解包。

## 凭据

```rust,ignore
session.add_s3_secret(S3Secret {
    scope: "s3://example-bucket/prefix/".into(),
    key_id, secret, session_token: None,
    region: "us-east-1".into(),
    endpoint: Some("s3.example.org".into()), // 无 scheme
    path_style: true,
    use_ssl: true,
})?;
session.add_http_secret(HttpSecret {
    scope: "https://example.org/private/".into(),
    headers: vec![("Authorization".into(), bearer_token)],
})?;
```

每个 Session 独占 instance/connection，secret 使用临时 `IN connection` 存储，并隔离 secret/spill 目录。关闭持久 secret、扩展自动安装/自动加载，保留证书校验。没有自动 credential-chain 入口；系统 GDAL 的配置也不会自动传入此后端。scope 仅用于选择凭据，不是资源访问控制。scope 必须使用显式 ASCII host/bucket（国际域名先转 punycode），拒绝 userinfo；仅含 authority 的 scope 自动补 `/`，避免误匹配主机、端口或 bucket 前缀。显式路径仍按前缀匹配。

当前 httpfs 会跟随重定向，且没有公开的关闭设置。实测跨源重定向会转发自定义 HTTP headers 和 S3 session token，因此 API 在创建 secret 前拒绝这些配置：HTTP 只接受一个 `Authorization` 头，S3 的 `session_token` 必须为 `None`。需要临时授权时，可由调用方生成预签名 HTTPS URL。恢复完整 header/临时凭据支持需要先具备可验证的原生重定向策略。

来源 Debug 和原生错误 Display/Debug 不输出 URL/SQL。`Error::sensitive_diagnostic()` 是显式的原生诊断入口，可能含 SQL、属性值和预签名 URL；仅交可信操作者。创建 secret 失败时完全丢弃原生诊断，避免密码出现在错误中。

## SQL 与数据契约

`session.query(sql, parameters)` 接受可信调用方的 SELECT，可用 CTE 组合处理。它不是不可信 SQL 沙箱。修补扩展中的 `ST_Read`/`ST_Read_Meta` 也会传播读取错误。使用未经修补的官方模式时，不应绕过 reader 限制直接调用这两个原生函数。高级 SQL 不会被解析或改写。值使用重导出的 `Value`；整数、decimal、时间和嵌套字段沿用 DuckDB/Arrow 类型，不先转 JSON 或 f64。

| 处理 | SQL 形态 | 语义 |
| --- | --- | --- |
| Filter | `WHERE class = ?` | 参数绑定，不保证顺序 |
| Reproject | `ST_Transform(geom, 'EPSG:...')` | 输入需要明确 CRS；Session 固定 x/y 轴顺序 |
| Buffer | `ST_Buffer(geom, ?)` | 平面距离使用坐标单位；米制计算先选择合适投影 |
| Clip | `JOIN ... ON ST_Intersects(...)` + `ST_Intersection` | 按匹配掩膜输出；重叠掩膜可以产生多行 |
| SpatialJoin | `JOIN/LEFT JOIN ... ON ST_Intersects/Within(...)` | Intersects 包含边界；一对多与多对多保留 |
| Dissolve | `GROUP BY ...` + `ST_Union_Agg(geom)` | 其他属性必须明确聚合；并非完整 GIS Union Overlay |

几何列的 Arrow metadata 携带 PROJJSON；`VectorMetadata` 单独列出全部几何列及 CRS。未知 CRS 为 `None`。属性读取允许未知 CRS；SQL 调用方负责空间算法的 CRS、维度、单位及输入合法性，不自动修复、降维或重投影。跨来源计算没有事务快照，文件在作业期间必须稳定。

原生 GeoParquet reader 当前接收 1.x WKB；拒绝未验收的 native encoding、原生 Parquet GEOMETRY/GEOGRAPHY 逻辑注解、spherical edges、坐标 epoch 和 M 类型声明。原生注解可能与 `geo` metadata 提供不同 CRS，当前不选择其中一个隐式解释。普通 Parquet 缺少 `geo` metadata 不会被当成 GeoParquet。高级 SQL 的直接 reader 调用由调用方承担同等校验。

GeoParquet 输出只接受根级几何列；递归拒绝 List/Struct/Map 等嵌套字段中的几何。普通嵌套属性仍可保留。`TIMETZ` 因 Arrow 会丢弃偏移，在 schema/批次/文件输出前按原生 logical type 递归拒绝；普通 `TIME` 与 `TIMESTAMPTZ` 不受此限制。`INTERVAL` 在 Arrow 中可保留微秒，但 Parquet 编码会截断到毫秒，因此仅文件输出递归拒绝。需要这些列时，由调用方显式转换成可接受的表示。

本地输出是 **GeoParquet 1.1 WKB**。2.0 原生 V1 自动元数据在实测中会遗漏未知 CRS/全 NULL 几何列，因此输出使用原生 Parquet COPY、`ST_AsWKB` 和显式 `KV_METADATA.geo`，将未知 CRS 写成 JSON null，保留所有顶层几何列。第一几何列为 primary；`geometry_types: []` 表示未知，省略 bbox，不额外扫描来源统计类型/范围。该表示符合 [GeoParquet 1.1](https://geoparquet.org/releases/v1.1.0/)；后续可按实测增加统计量。M 坐标在写入时明确拒绝。

输出在同文件系统临时目录完成关闭、metadata/字段类型/CRS 回读后，使用 hard link 原子创建新路径；已有文件、软链接、硬链接目标均不会覆盖。目标文件系统需支持 hard link；不支持时返回错误。此保证不是断电持久化承诺。普通失败清理临时输出；`PublishedCleanup` 明确表示结果已发布但临时目录清理失败；`Cleanup` 同时保留操作与清理错误。

## 流式消费、取消与资源

```rust,ignore
let report = session.query("SELECT id, geom FROM roads", [])
    .for_each_batch(&control, |batch| {
        consume(batch).map_err(|e| Error::Consumer(Box::new(e)))
    })?;
```

使用 `Statement::stream_arrow`，DuckDB 报出的每批 fetch/转换错误都会传播（无法恢复原生扩展已经吞掉的错误），零行结果也返回 schema。仅 Ok 的最终报告表示整条查询完成；调用方已经消费的前缀不会自动回滚。回调错误立即终止；回调 panic 在 Rust 中展开并清理监听。重导出的 `arrow` 对应实际 binding 版本，避免消费方误用不兼容 Arrow 类型。

同步调用放在 blocking 线程；不在库里创建 Tokio runtime/连接池。需要异步时由应用 `spawn_blocking`，传入已有 `Cancellation`。每次带取消 token 的操作有一个受控监听线程，结束/展开前停止并 join，再复用连接。取消后仍须等待 blocking 工作退出；原生网络/驱动 I/O 不保证硬实时中断。

`register` 在事务中创建 view，最终取消检查通过后才提交；返回 `Cancelled` 时会回滚本次注册，同名旧关系不会被删除。提交成功后晚到的取消不改变成功结果。`Rollback` 表示回滚未确认，同时保存原始与回滚错误；此时调用方应关闭 Session，不假定可安全重试。

Session 默认 2 线程、512 MiB buffer-manager、1 GiB spill、30 秒 HTTP 超时、2 次重试，可通过 `SessionConfig` 修改。`memory_limit_bytes` 不是 RSS 硬限制，排序/聚合/空间 join 仍可能物化。流式 `max_bytes` 累计计算 Arrow buffer，文件 `max_bytes` 在 COPY 完成后按编码大小检查；它们均不限制消费方保留的数据或临时文件峰值。写文件的 max_rows 读取最多预算+1 行后拒绝超额，不静默截断并报告成功。

验证方法及未测范围见 [测试说明](TESTING.md) 与 [实测记录](DUCKDB_VALIDATION.md)。PostGIS 目前可组合现有文件 writer（系统 GDAL 需要对应 driver）；直接 Arrow→PostGIS 流式适配尚未实现。

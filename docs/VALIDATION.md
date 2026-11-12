# 提取基线验证记录

本页保留公开前的本地提取与审查历史记录。所列旧提交属于本地审查档案，不在公开 Git 历史中；“未公开”等状态是当时的观察。公开快照与后续验证见 [公开说明](PUBLICATION.md)。

日期：2026-11-12。验证的实现提交为 `278a62b762d300e26b48f5d963b727e1f7a62246`，导入对比基线为 `aba9a93484517dc7787bcb08a695d9b66e47480d`。这是独立提取实现的验证记录，不代表后续审查修复已被这一轮覆盖。

## 已实跑通过

- macOS arm64，Rust/Cargo 1.98.1，库侧 GDAL 3.12.2。
- `bash scripts/ci/check.sh`：结构/测试归属、fmt、Clippy 全 targets 且 `-D warnings`、54 个默认 tests、3 个 doctests、Rustdoc 且 `-D warnings`、自包含转换示例、`git diff --check`。
- `cargo package --offline --locked`：生成 .crate 并验证规范化后的独立包可以编译。未发布。没有仓库归属 URL，因此 Cargo 的 homepage/repository/documentation 元数据提醒保留，未虚构远端地址。
- 对实际打包展开目录使用新的 target 重新执行 `cargo test --offline --locked --manifest-path .../Cargo.toml`：54 tests + 3 doctests 通过；12 个数据库项仍按约定 ignored。
- 12 个数据库回归全部显式执行：12 passed / 0 failed / 0 ignored。使用专门新建、仅私有 Unix socket 可达的 PostgreSQL 17.10 + PostGIS 3.6.4，未连接业务库。PostGIS raster 需要其安装时链接的 GDAL 3.13.1 及对应依赖，全部使用本机已有版本，仅为这个测试进程设置动态库搜索路径，没有改系统库或全局链接。
- 未提供 DSN 时 `scripts/ci/test-postgis.sh` 确实返回失败；没有默认连接或跳过成功。
- 185 个外部锁定包的版本、source 和 checksum 与来源 MapSeek Cargo.lock 一致。
- CI YAML 可解析，两个 shell 入口通过 `bash -n`。
- 来源 MapSeek `python3 scripts/ci/test-naming.py`：895 个测试文件、884 个源码 owner、11 个显式跨模块套件、0 错误。原仓库工作树仍干净。

## 消费者接入验证

从原提交导出相关 Rust 服务、共享 crate 和 IDL 到隔离快照，不修改原 MapSeek。仅在快照中移除内嵌 core 的 workspace member，并把 spatial-engine、maptile-server 的 core path 改为独立项目。`cargo metadata --locked` 确认图中只有一个 spatial-core，路径为独立项目。

- `cargo check --offline --locked -p spatial-engine` 通过，确实检查了独立 spatial-core。使用原有构建 cache 加速依赖，不是完整服务运行、端到端或重新部署验证。
- maptile-server 只有 manifest 中的 spatial-core 依赖，当前手写源码没有 `spatial_core` 调用。完整 server check 未完成：首次快照漏带 IDL 导致其生成步骤失败，已补齐；联合检查中的 MapLibre Native 源码准备仍未结束，随后只终止了本次联合构建并单独完成 spatial-engine。没有使用跳过 native 编译的环境开关冒充 server 通过。
- 尝试在原 workspace 直接使用 Cargo `paths` override 会发生同名成员冲突，故正式迁移不能依赖这个捷径。

## 尚未验证或未执行

远端 CI、Linux/Windows 实测、完整 maptile-server 原生构建、服务启动/Temporal/DAG、真实 S3 与 TLS PostgreSQL、生产负载、GeoParquet 数据保真。当前本机存在 Parquet driver，但默认 suite 没有验证 GeoParquet 导出保真。

没有修改 MapSeek 原副本/依赖，没有创建远端、推送、公开、更换许可、发布 crates.io 或合并任何代码。独立项目可开发与 MapSeek 已迁移是不同结论。

## 审查修复验证

修复提交：`f25c0e9c17a1ba78d33062098d2f74ac05f9d91c`。归属说明提交：`c40c6d215ee5cfef994650c7041e525bf856becf`。以下记录补充前述基线，不改写基线当时的测试结果。

- 用 reviewer 的独立 harness 和精确 Cargo.lock 再次复现原缺陷：中途读取错误返回 `Ok(())` 并提交一行；首次读取失败的空矢量入口返回 `Ok(())` 并提交零行。
- 新增两个真实数据库回归，先在未修复生产代码上得到 0 passed / 2 failed；修复后 2/2 通过，明确核对旧表 sentinel 字段与内容保留、重复失败后的连接可用，以及有效/真正空输入重试。
- 完整 `bash scripts/ci/check.sh`：56 默认 tests、3 doctests、fmt、Clippy、Rustdoc、示例、结构和 diff 检查通过。默认 14 个数据库项仍为 ignored，不冒充数据库通过。
- `bash scripts/ci/test-postgis.sh`：14 passed / 0 failed / 0 ignored，使用同一隔离 PostGIS 测试实例。
- 独立 reviewer harness 再运行时两种读错误均返回 CPL Err；旧数据保留，原栅格往返探针也仍成功。
- georust/gdal v0.19.0 MIT 原文的 Git blob 与可靠来源一致：`eaaee8606f2f7d38283151b49d4478c8f2702cda`。未更改根 LICENSE 或推断全项目 MIT。

这里的通过范围仍不包含其他 GDAL 读写路径的穷尽审计、真实 S3、TLS、跨平台或生产负载。

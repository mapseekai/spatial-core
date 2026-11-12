# 提取来源、边界与接入

提取日期：2026-11-12。

## 可追溯基线

- 来源仓库：MapSeek，本地提交 `7f67b456ff412f3078877daaccf8997be03b2cfe`。
- 来源路径：`services/spatial-forge/spatial-core`，Git tree `aac005b062b8f344cd01522e69dfe753c29b5d64`。
- 本地提取档案的初始导入提交为 `aba9a93484517dc7787bcb08a695d9b66e47480d`，最终审查基线为 `73730260930ea3b789d57cffb41e253e1ed1618e`。这些追溯标识不属于本公开仓库的提交历史。
- 公开仓库从最终审查代码树建立独立初始提交，不携带旧测试环境配置所在的导入历史、MapSeek 完整历史、其他服务、用户配置或构建产物。原 MapSeek 和本地审查档案保留不动，见 [公开说明](PUBLICATION.md)。
- Cargo.lock 从同一来源提交的工作区锁文件缩减为本 crate 所需依赖，保留选中包的版本及校验和，没有升级依赖。lint 改为本地声明。

## 这次做了什么

独立 manifest、锁文件、工具链、CI、构建入口、示例、文档和结构/测试归属检查。保留 crate 名称、版本、API 与既有 GIS 算法/数据语义；新增加的 `apply_gdal_options` 是显式配置入口，不替换旧入口。兼容测试用子进程验证旧 CWD/TOML/OnceCell 行为，以及新入口不会自动读取 TOML。

数据库测试统一使用 `SPATIAL_CORE_TEST_POSTGRES_DSN`。外部手工准备的 GeoJSON 与硬编码本地数据库连接改成仓库自带合成 fixture 及专用 DSN；保留忽略标记、原有断言和清理意图，并将行数断言收紧为完整的三行导入。两处缺 DSN 时提前成功的测试改为报错。

## 分层

- spatial-core：数据格式、CRS/像元/几何计算、转换、数据库技术适配、取消/进度原语。
- 未来 server：算子目录、参数与执行协议、服务入口、资源解析和任务执行装配。
- gis-workflow：完整 DAG、依赖关系、调度、重试与业务编排。

没有搬入 `spatial-engine/src/operator`、`processing/workflow_operator`、Temporal、鉴权、服务发现或业务对象。`pg::VectorCopyContract` 是空矢量/字段类型写入契约，不是工作流/算子目录。

## 当前保留的契约与后续评估项

独立项目并不自动意味着所有旧约定已泛化。为避免这次提取改变生产行为，下列点保留：

1. `apply_gdal_config` 的应用 CWD TOML 自动读取及进程缓存；`None` 不清空已有线程 S3 凭据。明确配置 API 适用于新调用者，旧行为收敛需单独迁移与并发凭据回归。
2. 矢量 COPY 只允许目标 4326/3857，默认 `_id`/`geom`、bigint identity、GiST/ANALYZE，以及用于现有消费模式的文本 ID 游标索引。
3. `to_geoparquet` 保留 4326、2D、`FID=_id`、makevalid 和 skipfailures；这意味着它不保证保留所有输入要素/维度。需要无损或其他 CRS 时不能把它当作任意格式的无损转换器。
4. PostgreSQL 查询取消仍用原有 NoTls cancel 路径；外部 TLS 数据库、真实对象存储、负载及跨平台行为未因本地单测自动得到验证。

## MapSeek 接入是另一步

当前两个直接 Cargo 消费者是 `services/spatial-forge/spatial-engine` 和 `services/maptile/maptile-server`。它们仍引用 MapSeek 内部 path，根 workspace、CI 和 `services/gis-workflow/scripts/verify.sh` 仍指向原 crate。本次没有删除副本、改依赖或新增兼容 facade。

正式接入前：

1. 仓库归属现为 mapseekai；确认调用方的使用与分发权限后，选择固定 Git rev、私有 registry 版本或受管理的 vendoring。不能把本机 `../spatial-core` 当作团队/CI 可复现的正式依赖。
2. 在独立 MapSeek 迁移分支同时调整两个消费者、workspace members、Cargo.lock、CI 路径/包选择、验证脚本和相关文档。
3. 核对 GDAL 配置来源及 S3 线程生命周期，保留输入错误、取消、字段类型和事务契约；执行消费者及真实数据库回归。
4. 完成迁移验证后再决定删除内嵌副本。不要直接用 Cargo `paths` override 覆盖当前同名 workspace 成员；本次实际尝试会产生解析冲突。兼容性验证应使用隔离的源码快照，移除其内嵌 core 成员并同时修改两个消费者，不能把这种临时本地 path 当作已完成的正式接入。

## 许可与发布

原 LICENSE 原样保留。经所有者授权，源码发布到公开 GitHub 仓库 mapseekai/spatial-core；没有选择开源许可或发布 crate。另一个项目的 MIT 许可不能自动应用于本项目。

## 审查后的既有缺陷修复

独立提取之后的审查确认原代码中的两个数据完整性缺陷：OGR 迭代器将原生读取失败表现为 `None`，导致普通 COPY 将部分数据提交为替表结果，空矢量入口则可能误判为空数据而清空目标表。本地审查档案中的修复提交 `f25c0e9c17a1ba78d33062098d2f74ac05f9d91c` 在相关 lazy layer、计数/迭代器初始化及每次读取边界检查线程局部 CPL 错误，使失败进入原有拒绝或回滚路径。它是对既有失败行为的改善，不属于“提取完全零行为变化”的声明。

回归先在旧实现复现失败，再验证旧表内容与结构保留、连接可继续使用、正常输入重试成功，以及真正空输入仍可接受。相关代码未回写原 MapSeek。本次没有统一改写其他 GDAL 算法/格式转换入口，也没有改变配置默认语义或 S3 线程凭据生命周期。

继承的 GDAL helper 与 georust/gdal 可核对的重叠及上游原始声明，见 [第三方说明](third-party-notices.md)。这不改变项目自有材料的根许可证。

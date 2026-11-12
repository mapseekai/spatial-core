# 公开快照与审查基线

日期：2026-11-12。所有者已授权在 GitHub 组织 mapseekai 下公开本项目。

## 代码与历史

- 仓库：<https://github.com/mapseekai/spatial-core>。
- 公开初始提交来自最终本地审查基线 `73730260930ea3b789d57cffb41e253e1ed1618e` 的跟踪文件树，而不是导入旧 Git 历史。
- `src` 的 Git tree 为 `4660414ca58466c28384b3c4a0b5d6171b6a3d34`，与最终审查基线完全一致。Cargo.lock、测试 fixture、示例、脚本、工具链和 CI 也没有改变。
- 公开准备只新增仓库元数据和公开状态/来源文档。原来的 6 个本地提交及工作树保留为审查档案，没有改写或删除；MapSeek 工作树、依赖及远端均未修改。
- 不携带原始导入历史，避免传播已被移除的旧测试环境配置。只发布跟踪的源码、合成数据和项目文档；不包含本地日志、用户配置、个人数据、数据库或构建产物。

## 验证范围

公开前在 macOS arm64、Rust 1.98.1、GDAL 3.12.2 上重新执行 `scripts/ci/check.sh`：56 个默认测试及 3 个 doctest 通过，格式、Clippy、结构、Rustdoc 和可运行示例通过。14 个数据库项在默认运行中 ignored，不据此声称数据库验证通过。

同一生产源码的 14/14 PostGIS 回归已在审查阶段使用专用隔离实例全部通过，包括原生矢量首次/中途读失败的回滚。该测试实例已清理；公开准备没有重新运行数据库项。审查修复前后证据和消费者验证边界见 [历史验证记录](VALIDATION.md)。

GitHub CI 只以对应提交的实际 [Actions 结果](https://github.com/mapseekai/spatial-core/actions) 为准；本页不预先宣称远端平台通过。Windows、真实 S3/TLS PostgreSQL、完整服务运行及生产负载仍未验证。

## 许可与发布范围

根 LICENSE 与来源项目保持相同的 All rights reserved 声明；公开可见不等于开源许可。未将本项目改为 MIT，未发布到 crates.io，`publish = false` 保留。已识别的 georust/gdal 片段保留其完整上游 MIT 许可与版权声明，适用范围见 [第三方说明](third-party-notices.md)。

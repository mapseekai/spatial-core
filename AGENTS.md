# spatial-core 开发约定

本项目是独立的 Rust GIS library crate。先读 README.md；边界与迁移见 docs/EXTRACTION.md。

- 库只提供 GIS 数据读取、计算、转换及 PostGIS 写入能力。算子目录、HTTP/Temporal、业务身份、任务状态和 DAG 不属于本库。
- 保留现有公共 API 与数据语义。当前兼容配置入口会读取 CWD 下的 TOML，不能在普通重构中悄悄改变默认行为；新调用者优先显式配置。
- Rust 文件使用 snake_case。私有测试放在对应源码内；独立测试镜像 src 路径，同一源码最多一份独立测试文件。
- 修改后运行 bash scripts/ci/check.sh；数据库变更还需专用 PostGIS 测试库，见 docs/TESTING.md。缺失环境、忽略项和未执行项不能计为通过。
- 保留 LICENSE 和第三方归属声明。GitHub 公开可见不等于开源许可；修改远端归属、再次发布、改许可及 crates.io 发布仍需明确授权。
- 不通过用户级 Cargo 配置、相邻 MapSeek 源码或预存构建产物掩盖独立构建问题。

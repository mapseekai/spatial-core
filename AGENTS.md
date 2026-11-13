# spatial-core 开发约定

本项目是独立的 Rust GIS library crate。先读 README.md；边界与迁移见 docs/EXTRACTION.md。

- 库只提供 GIS 数据读取、计算、转换及 PostGIS 写入能力。算子目录、HTTP/Temporal、业务身份、任务状态和 DAG 不属于本库。
- 普通重构保留现有公共 API 与数据语义。PostGIS 的已授权破坏性迁移见 docs/POSTGIS_REFACTOR.md；当前入口为 postgis 的两类 writer，使用显式 GDAL 配置，不读取 CWD 下的 TOML。
- Rust 文件使用 snake_case。私有测试放在对应源码内；独立测试镜像 src 路径，同一源码最多一份独立测试文件。
- 修改后运行 bash scripts/ci/check.sh；数据库变更还需专用 PostGIS 测试库，见 docs/TESTING.md。缺失环境、忽略项和未执行项不能计为通过。
- 保留 LICENSE 和第三方归属声明。GitHub 公开可见不等于开源许可；修改远端归属、再次发布、改许可及 crates.io 发布仍需明确授权。
- 不通过用户级 Cargo 配置、相邻 MapSeek 源码或预存构建产物掩盖独立构建问题。

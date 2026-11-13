# 第三方依赖与许可

项目原始材料仍受根 LICENSE 的 All rights reserved 声明约束。该声明不改变第三方依赖的原始版权与许可。

本项目通过 Cargo 引用第三方 Rust 包，通过系统安装链接 GDAL、PROJ、GEOS 等 native 组件，没有将它们的完整源码或二进制 vendoring 进来。精确 Rust 版本/校验和见 Cargo.lock；已安装 GDAL 的驱动和 native 依赖随发行版变化。分发构建产物前需依据实际构建依赖附带适用的版权与许可，本页不是完整的二进制分发许可集合。

关键上游：

- GDAL：<https://gdal.org/en/stable/license.html>
- gdal / gdal-sys Rust 绑定：<https://github.com/georust/gdal>
- rust-postgres：<https://github.com/sfackler/rust-postgres>
- deadpool：<https://github.com/deadpool-rs/deadpool>
- Tokio：<https://github.com/tokio-rs/tokio>
- wkb-raster：<https://crates.io/crates/wkb-raster>

MapSeek 原仓库的 MapLibre Native 声明属于其渲染服务；本次提取没有包含 MapLibre Native 或 maptile-core。

## 已识别的 georust/gdal 代码片段

本项目继承的 GDAL 扩展包含与 [georust/gdal v0.19.0](https://github.com/georust/gdal/tree/v0.19.0) 实现一致或经调整的片段。为保留适用上游声明，随附该版本的完整 [MIT 许可及版权声明](../third_party/georust-gdal/LICENSE.txt)，Copyright (c) 2014 Alex Morega。

本次可核对的范围：

- `src/gdal_ext/destination.rs` 的枚举、转换、构造及所有权/drop 标志逻辑，对应上游 [`src/programs/raster/mdimtranslate.rs`](https://github.com/georust/gdal/blob/v0.19.0/src/programs/raster/mdimtranslate.rs) 的 `MultiDimTranslateDestination`；本地改名并增加安全说明。
- `src/gdal_ext/vector/translate.rs` 的 options 包装、argv 转换及目标/句柄转换结构，与该上游文件的 options 和 translate 包装有改编重叠；本地用于不同 GDAL API，且增加了边界检查和回归，不是整个上游文件的原样副本。
- `src/gdal_ext/ffi.rs` 的既有路径/空指针错误辅助函数，对应上游 [`src/utils.rs`](https://github.com/georust/gdal/blob/v0.19.0/src/utils.rs) 的同类帮助函数；本地错误消息复制与重置顺序已修正。

v0.19.0 是这次能够验证的比较快照，不据此断言最初引入版本。原 MapSeek 提交已含这些片段。此声明仅保留相关第三方片段的权利，不把整个 spatial-core 或本地新增代码改为 MIT，也不是其他未核实来源的完整审计。根 LICENSE 保持原样。

许可正文来源：[固定版本 LICENSE.txt](https://github.com/georust/gdal/blob/v0.19.0/LICENSE.txt)，Git blob `eaaee8606f2f7d38283151b49d4478c8f2702cda`。

# 性能分析工具

本目录包含项目自有的性能测试框架和结果分析工具。它有意位于内核 Cargo
workspace 之外。规划中的 `hitoshizuku-bench` 仓库将与
`tools/qemu-plugins`、性能分析脚本和 KCSAN 辅助工具一起接管本目录。
它消费带标签的内核构建产物和外部提供的输入镜像，不提供内核依赖。

在仓库拆分完成前保留这些源码，以保留性能测试历史。新的内核构建使用
`cargo xtask build`；仍依赖旧 shell 编排的脚本列入 `hitoshizuku-bench`
的迁移队列。平台性能测试需要外部 native 构建器时，设置
`HITOSHIZUKU_NATIVE_BUILDER`；该构建器应由独立的 `hitoshizuku-native`
仓库提供 `objects` 子命令。

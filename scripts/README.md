# 项目脚本

这些脚本是源码，不是 Cargo 依赖。项目拆分为多个职责明确的仓库期间，源码
暂时保留在此处：

- 内核构建和 KCSAN 辅助脚本留在内核工具中；
- 性能分析、QEMU 插件和分析脚本迁移到 `hitoshizuku-bench`；
- BusyBox、rootfs、镜像和 initramfs 辅助脚本迁移到未来的
  `hitoshizuku-initramfs` 仓库。

不再恢复旧的根目录编排。内核构建使用 `cargo xtask`；仍调用旧流程的脚本
列入未来负责仓库的迁移任务。

# xtask：内核工程入口

`xtask` 是根 workspace 的 Cargo 工程命令，替代旧的 Makefile 编排。它只编排内核
源码、ELM 工具和驱动配置，不生成 initramfs，也不下载或管理第三方 rootfs。

## 命令

```sh
cargo xtask config [--mode oldconfig|defconfig|config]
cargo xtask modules --target loongarch64-unknown-none
cargo xtask build --target loongarch64-unknown-none
cargo xtask clean
```

`build` 会先构建 kernel，再用 `cargo-elm profile-export` 导出接口，最后按
`drivers/Modules.toml` 构建模块。所有子进程共享 `target/<arch>` 和
`build/elm-interface/<arch>`，这样 Cargo 指纹可以复用公共依赖。

执行 `cargo xtask` 的当前目录必须是内核仓库根目录。跨目录调用 `cargo-elm` 时设置
`HITOSHIZUKU_KERNEL_ROOT`。

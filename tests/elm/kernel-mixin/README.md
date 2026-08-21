# 内核符号级 Mixin 运行测试

该工程验证真实 `allocator.GlobalAlloc.alloc` 的 `head`、参数修改、覆盖链和 `return`
站点，不使用测试 shim。正常镜像、成功替换镜像和迁移拒绝镜像均由同一份源码产生。

先为目标内核导出 `hitoshizuku-default` 接口包，然后依次构建三种镜像：

```sh
export ELM_KERNEL_INTERFACE_ROOT="$PWD/build/elm-interface-current"
ELM_TOOL="$PWD/build/cargo-elm-target/x86_64-unknown-linux-gnu/release/cargo-elm"

"$ELM_TOOL" elm build tests/elm/kernel-mixin --arch loongarch64 --unsigned
cp tests/elm/kernel-mixin/dist/test.kernel-mixin-loongarch64.eki build/kernel-mixin-v1.eki

"$ELM_TOOL" elm build tests/elm/kernel-mixin --arch loongarch64 --unsigned \
  --features replacement
cp tests/elm/kernel-mixin/dist/test.kernel-mixin-loongarch64.eki build/kernel-mixin-v2.eki

"$ELM_TOOL" elm build tests/elm/kernel-mixin --arch loongarch64 --unsigned \
  --features replacement,reject-migration
cp tests/elm/kernel-mixin/dist/test.kernel-mixin-loongarch64.eki build/kernel-mixin-reject.eki
```

把镜像放入 initramfs，并在允许不可信测试镜像的测试内核中执行：

```sh
elmctl load-eki /tmp/kernel-mixin-v1.eki
elmctl pause 3
elmctl snapshot
elmctl resume 3
elmctl snapshot
elmctl replace-eki 3 /tmp/kernel-mixin-reject.eki
elmctl snapshot
elmctl replace-eki 3 /tmp/kernel-mixin-v2.eki
elmctl snapshot
elmctl detach 3
```

输出必须能区分 v1 路由恢复和 v2 路由提交。迁移拒绝后仍应再次看到 v1 的四个分配
站点事件；成功替换后应看到 v2 事件；摘除后不得再进入该镜像处理器。

仓库中的 `scripts/elm-kernel-mixin-qemu-init.sh` 可以临时安装为 init，自动执行上述
事务并打印 `PASS`。该脚本只用于测试构建，不应进入正常发布 initramfs。

# profiling

内核内部事件、任务切换和 syscall 画像接口。运行时只记录可控的事件数据；统计学习和
模型拟合位于独立 `hitoshizuku-bench` 仓库。

```sh
cargo check -p profiling --target loongarch64-unknown-none
```

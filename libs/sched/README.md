# sched

任务、线程组、调度上下文和用户 ABI personality。调度 primitive 不负责启动用户态
镜像，执行加载由 kernel/general 协作完成。

```sh
cargo check -p sched --target loongarch64-unknown-none
```

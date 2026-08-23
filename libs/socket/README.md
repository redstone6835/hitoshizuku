# socket

套接字对象、协议族和 endpoint 生命周期。网络协议状态由 `net`/`net-stack` 管理，
socket 只维护调用边界、缓冲区和所有权。

```sh
cargo test -p socket --target x86_64-unknown-linux-gnu
```

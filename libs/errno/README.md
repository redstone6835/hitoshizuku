# errno

内核和 Native ABI 共用的错误码类型。新增错误应保持数值稳定、明确失败类别，并同步
ABI registry；不要在调用点随意复用已有编号。

```sh
cargo test -p errno --target x86_64-unknown-linux-gnu
```

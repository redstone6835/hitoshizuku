# scripts/tests：KCSAN 脚本测试

核心仓库只保留 KCSAN wrapper/symbolizer 的测试。测试输入使用临时目录生成，不能依赖
`target/`、外部镜像或性能仓库的结果文件。

```sh
python3 -m unittest scripts.tests.test_kcsan_symbolize
LLVM_NM=/usr/bin/nm scripts/test-kcsan-codegen.sh
```

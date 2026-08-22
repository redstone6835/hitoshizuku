# KCSAN 辅助脚本

内核仓库只保留 KCSAN 编译包装器、符号定位器和对应的代码生成测试。性能画像、QEMU
插件、系统调用比较和机器学习模型位于独立的
[`hitoshizuku-bench`](https://github.com/redstone6835/hitoshizuku-bench) 仓库。

脚本从内核仓库根目录运行，并使用同一次构建生成的 map/符号文件。

```sh
scripts/test-kcsan-codegen.sh
python3 -m unittest scripts.tests.test_kcsan_symbolize
```

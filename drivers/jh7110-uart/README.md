StarFive JH7110 / DesignWare APB UART 驱动骨架

用途：实现对 `starfive,jh7110-uart` 与 `snps,dw-apb-uart` 兼容节点的支持，作为系统控制台和调试串口。

当前状态：骨架目录，需实现 `probe`、IRQ、clock/pinctrl 绑定与字符设备注册。

参考：设备树中的 compatible 字段见 `vf2-device-tree/vf2-firmware-fdt.dts`。

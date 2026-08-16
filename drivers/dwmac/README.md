Synopsys DW MAC Ethernet 驱动骨架

用途：实现 `snps,dwmac-5.20` / `starfive,jh7110-dwmac` 兼容的以太网 MAC 驱动，含 MDIO/PHY 支持。

当前状态：骨架目录，需实现 DMA/descriptor、PHY/MDIO 与 netdev 集成。

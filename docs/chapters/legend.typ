#import "../config.typ": legend-title
#import "../styles/diagram.typ": legend-color-cell, legend-color-table, legend-flow-demo
#import "../styles/figure.typ": asm-sample, continued-table, figure-caption, hexdump-sample, pseudo-sample
#import "../styles/navigation.typ": manual-front-section
#import "../styles/tokens.typ": handoff-fill, soft-fill, stable-fill, warm-fill

#manual-front-section(legend-title)[
  本技术手册采用统一的书写规则描述目录结构、控制权转移、数据布局、启动状态和硬件相关细节。凡例中的规则适用于正文所有章节，除非某个章节在局部明确给出了更严格的约束。凡例仅规定表达方式，不改变源代码中的模块边界、调用关系及实现事实。

  正文中的等宽字体表示需要精确引用的技术对象。入口符号、函数名、结构体名、寄存器名、文件格式字段、命令行参数和路径片段均使用等宽字体。例如，`StartContext` 表示一个精确的结构体名称，`_start` 表示一个精确的入口符号，`docs/main.typ` 表示一个精确的路径片段。

  图中的箭头用于表示控制权转移、依赖方向、数据流向或初始化顺序。箭头的含义由图题和相邻正文共同确定，不脱离上下文单独构成完整论证。图 凡例-1 展示了本手册最常见的控制流表示方式。

  #figure(caption: figure-caption("图", "凡例-1", [控制流箭头示例]))[
    #legend-flow-demo([固件入口], [上下文交接], [稳定状态])
  ]

  图表中的不同颜色用于提示职责类型。图 凡例-2 展示了本手册图表使用的颜色语义，相同的颜色在后继章节中保持相同的含义。

  #figure(caption: figure-caption("图", "凡例-2", [图表颜色语义]))[
    #legend-color-table((
      legend-color-cell([浅蓝色], [表示平台无关的公共能力、接口抽象和通用基础设施。], fill: soft-fill),
      legend-color-cell([浅橙色], [表示当前正处于掌握控制权或执行关键启动动作的阶段。], fill: warm-fill),
      legend-color-cell([浅绿色], [表示已经完成稳定化、可被后续阶段直接消费的数据或运行状态。], fill: stable-fill),
      legend-color-cell([浅紫色], [表示阶段之间的一次性 handoff、上下文传递和控制权交接。], fill: handoff-fill),
    ))
  ]

  图表或代码标题的格式为“图/表/代码 章号-序号 名称”。图、表、代码及编号用于定位图表或代码，名称用于概括内容。表 凡例-1 给出了标题及正文引用的书写示例。

  #continued-table(
    "凡例-1",
    [图表标号格式示例],
    (1.2fr, 2.2fr, 2fr),
    (
      table.cell(fill: soft-fill)[#text(weight: "bold")[对象]],
      table.cell(fill: soft-fill)[#text(weight: "bold")[标题格式]],
      table.cell(fill: soft-fill)[#text(weight: "bold")[引用方式]],
    ),
    (
      [图],
      [图 1-1 项目分层结构],
      [见图 1-1],
      [表],
      [表 1-1 职责边界],
      [见表 1-1],
      [代码],
      [代码 1-1 进入内核的流程],
      [见代码 1-1]
    ),
    kind: "表",
    continuation-kind: "续表",
    align: (left, left, left),
  )

  参考手册的结构讲解部分不会出现真实代码。描述流程或算法时，正文优先使用流程图；当流程图不足以表达条件分支、循环、状态变化或错误处理路径时，正文将使用伪代码。伪代码仅用于说明控制流和数据依赖关系，不与任何具体源文件实现绑定。例如，在描述 trait 定义或泛型算法等与 Rust 语言紧密相关的设计时，可能会使用 Rust 伪代码；在描述系统调用分发、设备驱动接口等与 C 语言风格更贴近的设计时，使用 C 伪代码。两种伪代码风格服务于同一目的，即表达设计意图而非实现细节，读者应关注伪代码所传达的逻辑，而非其语言形式。

  #pseudo-sample("凡例-1", [C 风格伪代码示例], kind: "代码")[
    ```c
    if (context.has_acpi) {
      parse_acpi_tables(context.acpi_root);
    } else if (context.has_dtb) {
      parse_dtb_blob(context.dtb_blob);
    } else {
      keep_platform_state_unparsed();
    }
    ```
  ]

  Rust 风格伪代码示例如下所示：

  #pseudo-sample("凡例-1b", [Rust 风格伪代码示例], kind: "代码")[
    ```rust
    trait CharDriver: Send + Sync {
        fn write(&self, buf: &[u8]) -> Result<usize, CharIoError>;
        fn read(&self, buf: &mut [u8]) -> Result<usize, CharIoError>;
        fn flush(&self) -> Result<(), CharIoError>;
        fn as_any(&self) -> &dyn Any;
    }
    ```
  ]

  在描述二进制文件、固件表、镜像片段、启动参数块或内存转储时，正文使用 xxd 风格的十六进制输出。十六进制转储应根据偏移、十六进制字节序列和必要注释来理解，字段长度、对齐方式和字节序由相邻正文说明。

  #hexdump-sample("凡例-2", [xxd 风格十六进制示例], kind: "代码")[
    ```hexdump
    00002520: 89d8 488d 0db8 2100 0099 f77c 2414 85d2    ..H...!....|$...
    00002530: 7514 488d 0da3 2100 0085 db48 8d05 9c21    u.H...!....H...!
    00002540: 0000 480f 44c8 4839 2d43 3b00 0048 8d05    ..H.D.H9-C;..H..
    00002550: 9921 0000 4589 e048 8d15 8621 0000 be01    .!..E..H...!....
    00002560: 0000 004c 89ef 480f 45d0 31c0 e80f eeff    ...L..H.E.1.....
    00002570: ff85 c00f 886c 0100 0083 c301 4863 c348    .....l......Hc.H
    00002580: 3b44 2418 0f8c 76ff ffff 807c 2420 000f    ;D$...v....|$ ..
    00002590: 856b ffff ff89 5c24 384c 8b64 2428 837c    .k....\$8L.d$(.|
    000025a0: 2438 0074 0f4c 89ee 488d 3d28 2100 00e8    $8.t.L..H.=(!...
    000025b0: 4c12 0000 4d85 e40f 8448 feff ff4c 89ee    L...M....H...L..
    000025c0: 488d 3df1 2100 00e8 3412 0000 e8cf edff    H.=.!...4.......
    000025d0: ff41 0fb6 1424 4889 c348 8b00 f644 5001    .A...$H..H...DP.
    ```
  ]
  汇编代码是“不出现真实代码”规则的例外情况。在必须解释入口序列、寄存器约定、CSR 操作、TLB 相关指令或异常入口时，正文会直接写出真实架构汇编。

  #asm-sample("凡例-3", [LoongArch64 汇编示例], kind: "代码")[
    ```asm
    li.d    $t0, 0x9000000000000000
    csrwr   $t0, 0x180
    jirl    $zero, $ra, 0
    ```
  ]
  地址和长度的描述区分虚拟地址、物理地址和文件偏移。除非特别说明，固件表中的地址指固件提供的物理地址；内核访问时的地址指经过早期映射或内核映射转换后的虚拟地址；二进制结构说明中的偏移指文件或缓冲区内部的位置。表 凡例-2 给出了这些记号的用法示例。

  #continued-table(
    "凡例-2",
    [地址和数值记号示例],
    (1.1fr, 1.7fr, 2.2fr),
    (
      table.cell(fill: soft-fill)[#text(weight: "bold")[记号]],
      table.cell(fill: soft-fill)[#text(weight: "bold")[示例]],
      table.cell(fill: soft-fill)[#text(weight: "bold")[含义]],
    ),
    (
      [虚拟地址],
      [0x9000_0000_1000_0000],
      [内核已经能够直接访问的映射地址。],
      [物理地址],
      [0x0000_0000_1000_0000],
      [固件表、内存图或页表项描述的机器地址。],
      [文件偏移],
      [00000010:],
      [二进制转储中相对于文件或缓冲区起点的位置。],
      [容量单位],
      [KiB、MiB、GiB],
      [二进制容量单位；KB、MB、GB 表示十进制容量。],
    ),
    kind: "表",
    continuation-kind: "续表",
    align: (left, left, left),
  )

  本手册使用“启动阶段”“运行期”“平台无关”“架构相关”“固件来源”和 `handoff` 等术语描述内核边界。表 凡例-3 给出了这些术语的固定含义，后文章节不再重复解释。

  #continued-table(
    "凡例-3",
    [核心术语说明],
    (1.2fr, 3fr),
    (
      table.cell(fill: soft-fill)[#text(weight: "bold")[术语]],
      table.cell(fill: soft-fill)[#text(weight: "bold")[含义]],
    ),
    (
      [启动阶段],
      [指进入 main 函数之前的控制权转移和基础设施建立过程。],
      [运行期],
      [指操作系统所需的各种基础能力建立之后的长期执行阶段。],
      [平台无关],
      [指不依赖具体指令集或特定固件入口的通用逻辑。],
      [架构相关],
      [指依赖具体处理器、指令集、异常模型或启动 ABI 等具体硬件架构的逻辑。],
      [固件来源],
      [指 ACPI 表、DTB、EFI 系统表或其他启动协议提供的平台信息。],
      [handoff],
      [指某一个阶段将一次性上下文交给下一阶段，并退出决策路径的过程。],
    ),
    kind: "表",
    continuation-kind: "续表",
    align: (left, left),
  )

  正文中的“必须”“应该”“可以”和“不会”表达不同强度的工程约束。表 凡例-4 说明了这些约束词的含义，以避免将实现建议误读为硬性接口条件。

  #continued-table(
    "凡例-4",
    [工程约束词说明],
    (1fr, 3.2fr),
    (
      table.cell(fill: soft-fill)[#text(weight: "bold")[词语]],
      table.cell(fill: soft-fill)[#text(weight: "bold")[含义]],
    ),
    (
      [必须],
      [违反该条件将导致接口失效、启动失败或语义不成立等设计缺陷。],
      [应该],
      [该规则为推荐边界，违反时需要给出明确理由。],
      [可以],
      [存在多种可接受的实现方案。],
      [不会],
      [当前设计明确排除了某一行为。],
    ),
    kind: "表",
    continuation-kind: "续表",
    align: (left, left),
  )

  章节中的图表、伪代码、十六进制转储和汇编片段均服务于对结构的解释，它们不能替代对真实实现的验证。涉及启动路径、页表、固件解析和分配器状态的结论，最终仍以仓库源码、构建结果和目标平台运行日志为准。
]

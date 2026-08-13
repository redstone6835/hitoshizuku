#import "../styles/diagram.typ": flow-arrow, layer-card
#import "../styles/figure.typ": continued-table, figure-caption, pseudo-sample
#import "../styles/tokens.typ": handoff-fill, soft-fill, stable-fill, warm-fill

= 第十四章 SOYO 与 MyGO Native ABI

前述章节以 Linux ELF、POSIX 系统调用和文件描述符为主线说明了兼容用户态。MyGO 同时提供一条独立的原生用户态路径：SOYO 负责描述可执行映像和共享组件，MyGO Native ABI 负责描述程序可以调用的操作、可以持有的对象能力以及进程启动时的交接数据。两者相互配合，但职责并不相同。SOYO 是文件格式，Native ABI 是运行时契约；同一份 Native ABI 也不会依赖 ELF 的动态链接语义。

原生路径不是把 Linux 系统调用重新编号，也不是用另一种容器包装 ELF。它采用显式导入、能力句柄和固定线格式，使映像在执行之前就能声明所需接口，内核在装载阶段完成兼容性和授权检查。Linux 兼容程序继续使用 Tomori Linux personality，SOYO 程序使用 MyGO Native personality。两种 personality 共用进程、调度、VFS、内存和网络等内核机制，但在用户态入口、对象命名与错误返回上保持隔离。

== 14.1 总体结构与职责边界

原生用户态由五个层次组成：

- `libs/soyo` 定义 SOYO 线格式、解析、结构校验、布局规划和信任验证；
- `libs/native-abi` 定义 ABI family、epoch、操作注册表、对象接口、权限、状态码、句柄和启动区固定布局；
- `tools/soyo-linker` 把目标架构的 ELF 可重定位对象直接链接为 SOYO，并生成与 manifest 一致的 C 或 Rust 绑定；
- `kernel/src/soyo.rs` 负责读取、验证、映射、重定位以及构造进程运行时布局；
- `kernel/src/native_abi` 负责 Native call 的分发、对象解析、权限检查和具体操作实现；`native/mrt`、`native/ranalib` 与 `native/anonlib` 则为 C 和 Rust 程序提供用户态运行时。

#figure(caption: figure-caption("图", "14-1", [SOYO 构建、装载与调用链]))[
  #layer-card("程序与 manifest", [C 或 Rust 源码声明实际逻辑，manifest 声明 ABI 导入、初始能力与运行时约束], fill: soft-fill)
  #flow-arrow(label: "生成绑定并编译")
  #layer-card("soyo-ld", [解析同架构 ET_REL 对象，完成段布局、符号解析和受限重定位，输出 SOYO], fill: warm-fill)
  #flow-arrow(label: "exec 装载")
  #layer-card("SOYO 装载器", [校验格式、摘要、架构、ABI、特性和资源上限，映射映像并构造 StartInfo], fill: handoff-fill)
  #flow-arrow(label: "进入用户态")
  #layer-card("MRT 与 Native call", [运行时验证启动契约，程序通过 Call Slot 和 capability handle 调用内核对象], fill: stable-fill)
]

这里有两条必须保持的边界。第一，manifest 是程序契约的唯一输入，生成的绑定和最终 SOYO 必须来自同一份 manifest。第二，程序只能调用已经导入的 operation，并且只能对内核授予的 capability handle 执行其权限允许的操作。链接器不会根据未解析符号猜测权限，内核也不会因为程序知道某个 operation 编号而扩大授权。

== 14.2 SOYO 对象容器

SOYO v1 使用 little-endian 固定线格式，文件以四字节 magic `soyo` 开始。头部给出产物类型、目标架构、ABI 身份、特性位、入口偏移、文件大小、映像虚拟大小、构建标识和内容摘要。头部之后是目录；目录项描述各张元数据表的位置、条目大小、条目数量和对齐。解析器逐字段读取固定宽度整数，不把磁盘字节直接转型为 Rust 结构体，因此宿主结构体布局不会成为隐含 ABI。

#continued-table(
  "14-1",
  [SOYO 主要元数据表],
  (1.45fr, 2.9fr),
  (
    table.cell(fill: soft-fill)[#text(weight: "bold")[表或记录]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[用途]],
  ),
  (
    [`String`], [保存诊断名称。名称便于检查工具输出，不参与运行时身份判定。],
    [`ImageSegment`], [描述代码、只读数据、可写数据、BSS 和 TLS 模板的文件范围、虚拟偏移、内存大小与权限。],
    [`AbiImport`], [按连续 Call Slot 声明 operation ID、必需性和签名哈希。],
    [`CapabilityRequirement`], [声明启动时需要的对象接口、最小权限和必需性。],
    [`Relocation`], [表达装载基址或段基址的 64 位重定位；目标只能位于非代码可写入段。],
    [`RuntimeInfo`], [描述栈、保护页、StartInfo 上限以及构造和析构数组。],
    [`Component*`], [共享组件使用的身份、依赖、接口导入导出、动态重定位和签名记录。],
  ),
  kind: "表",
  continuation-kind: "续表",
  align: (left, left),
)

SOYO 区分 `Executable` 与 `SharedComponent`。可执行映像必须把入口偏移放在代码段的文件内容范围内；共享组件没有进程入口，其头部入口偏移必须为零，初始化和终止入口由组件元数据单独描述。两种产物都可以包含代码、只读数据、数据、BSS 和 TLS 模板，但共享组件还要声明组件身份、ABI 身份、依赖和接口符号。

段权限采用固定的 W^X 组合。代码段只读且可执行，只读数据段只读，数据、BSS 和 TLS 模板可读写而不可执行。普通段按 4 KiB 页面布局，段间和文件尾的填充必须为零。装载器先在可写映射中填充段并应用重定位，随后再收紧到最终权限，避免在最终可执行页面上保留写权限。

格式还设置了显式资源上限。单个文件最大 256 MiB，映像虚拟空间最大 1 GiB；段、导入、能力、重定位和组件依赖分别有独立计数上限。所有偏移加法、数量乘法和对齐运算都执行溢出检查。未知的必需表、未知的必需 feature、非零保留字段、重叠范围、错误摘要和非规范填充都会导致装载拒绝，而不是被宽松忽略。

== 14.3 Native ABI 身份与 manifest

当前原生 ABI 的 family 为 MyGO Native，epoch 为 1。family 区分不兼容的 ABI 系列，epoch 表示同一系列中的机器契约版本。目标架构是 ABI 身份的一部分，RISC-V64 与 LoongArch64 映像不能交叉装载。operation 的稳定身份由数值 ID 和签名哈希共同确定：ID 用于定位操作，签名哈希绑定对象接口、参数、返回值和 epoch。只匹配 ID 而签名哈希不同的导入会被拒绝。

manifest 把程序需求分成三部分：

1. `imports` 声明程序会使用的 operation，例如 `process.exit`、`stream.write` 或 `memory.allocate`；
2. `capabilities` 声明进程启动时需要的对象，例如自身进程、当前地址空间、标准流、单调时钟或根目录，以及每个对象需要的权限；
3. `runtime` 声明栈大小、保护区、StartInfo 上限等装载约束。

#pseudo-sample("14-1", [原生程序 manifest 的最小结构], kind: "代码")[
  ```json
  {
    "manifest_version": 1,
    "abi_epoch": 1,
    "entry": "_start",
    "imports": [
      { "operation": "process.exit", "required": true },
      { "operation": "stream.write", "required": true }
    ],
    "capabilities": [
      { "requirement": "self_process", "rights": ["exit"], "required": true },
      { "requirement": "stdout", "rights": ["write"], "required": true }
    ],
    "runtime": {
      "stack_size": 65536,
      "stack_guard_size": 4096,
      "start_info_max_size": 4096
    }
  }
  ```
]

必需导入无法绑定时，映像不能执行；可选导入无法绑定时，对应 Call Slot 保持未绑定，调用会得到 `ABI_UNSUPPORTED_OPERATION`。能力声明也遵循最小授权原则：所请求接口必须与 requirement 的固定接口一致，权限必须是该 requirement 最大权限的子集。内核只授予实际可提供的能力，不用文件描述符编号或全局路径暗示额外访问权。

== 14.4 装载与启动交接

`exec` 路径先读取文件前缀。`soyo` magic 选择原生装载器，ELF magic 和 shebang 分别进入 Linux ELF 与脚本路径。SOYO 路径在修改当前任务之前完成读取、格式校验、ABI 绑定、段映射、重定位和运行时准备，最终形成一个完整的待提交映像。任何步骤失败都丢弃新地址空间，原任务不会进入半替换状态。

装载过程依次完成以下工作：

1. 通过有界随机读取解析头部、目录和各张表，并验证 build ID 与 content hash；
2. 检查产物类型、目标架构、required feature、ABI family、epoch、operation 和 capability 契约；
3. 为映像选择用户虚拟基址，映射各段，应用受限重定位并收紧段权限；
4. 规划用户栈、栈保护区、静态或动态 TLS 区以及只读 StartInfo 区，检查各范围不重叠；
5. 建立进程级 Native personality、Call Slot 绑定和 capability handle 表；
6. 构造 StartInfo，把入口寄存器、栈指针和线程指针写入目标架构陷阱帧，然后原子提交执行映像。

StartInfo 是内核到 MRT 的固定线格式交接区，magic 为 `syst`，当前版本为 1。它包含 ABI epoch、目标架构、启用特性、映像基址、页大小、TLS 信息、`argv`、`envp`、初始 handle、Call Slot 数量、随机种子以及构造和析构数组。所有内部引用都使用相对 StartInfo 起点的偏移，不使用内核地址。内核映射完成后把该区域改为用户只读，运行时只能读取而不能改写启动契约。

#continued-table(
  "14-2",
  [原生入口寄存器交接],
  (1.15fr, 1.25fr, 2.3fr),
  (
    table.cell(fill: soft-fill)[#text(weight: "bold")[参数]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[寄存器]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[含义]],
  ),
  (
    [参数 0], [`a0`], [只读 StartInfo 的用户虚拟地址。],
    [参数 1], [`a1`], [StartInfo 总长度。],
    [参数 2], [`a2`], [当前映像的装载基址。],
    [参数 3], [`a3`], [bootstrap self-process capability handle。],
    [线程指针], [`tp`], [无 TLS 时为零；启用 TLS 时指向初始 TLS 基址。],
  ),
  kind: "表",
  continuation-kind: "续表",
  align: (left, left, left),
)

MRT 进入程序前会再次校验 StartInfo。它检查 magic、版本、总长度、保留字段、ABI、架构、映像基址、页大小、Call Slot 数量、TLS、字符串范围、初始 handle 顺序和权限。C 入口随后建立 `argc`、`argv`、`envp` 和 `environ`，运行构造函数，调用 `main`，运行析构函数，最后通过 `process.exit` 终止。Rust 入口使用同一份启动契约，但由 `anonlib` 提供 `no_std` 侧的安全封装。

== 14.5 Native call 与返回约定

Native call 使用与 Linux syscall 相同的陷阱指令，但寄存器内容和分发表由任务 personality 决定。RISC-V64 执行 `ecall`，LoongArch64 执行 `syscall 0`。异常入口检查当前线程组的 `UserAbiKind`：Tomori Linux 任务进入 Linux syscall 表，MyGO Native 任务进入 Native dispatcher。因此，一个 SOYO 程序不能用 Native Call Slot 意外调用 Linux syscall，Linux 程序也不会因为相同寄存器值进入 Native operation。

#continued-table(
  "14-3",
  [Native call 寄存器约定],
  (1.25fr, 1.1fr, 2.4fr),
  (
    table.cell(fill: soft-fill)[#text(weight: "bold")[方向]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[寄存器]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[内容]],
  ),
  (
    [调用], [`a7`], [manifest 中连续编号的 Call Slot，而不是全局 operation ID。],
    [调用], [`a6`], [目标 capability handle。],
    [调用], [`a0` 至 `a4`], [最多五个 64 位 operation 参数。未使用参数必须为零。],
    [调用], [`a5`], [保留参数，epoch 1 中必须为零。],
    [返回], [`a0`], [32 位 Native status。零表示成功。],
    [返回], [`a1`、`a2`], [两个 64 位结果值；失败时内核强制清零。],
  ),
  kind: "表",
  continuation-kind: "续表",
  align: (left, left, left),
)

内核分发顺序是 ABI 的一部分。分发器先检查 Call Slot 和绑定状态，再检查保留参数与未使用参数，然后解析 handle 的接口和权限，最后执行 operation。这样可以保证错误分类稳定，也避免在参数本身不规范时触碰用户指针或对象状态。阻塞操作若遇到外部终止或映像替换，会在统一安全边界处理进程控制；需要继续执行时重新尝试原调用，而不会向用户态暴露半完成状态。

Native status 不使用 Linux 的负 errno。状态码按类别划分，例如 ABI、handle、安全、流、内存、进程、映像、事件、组件、线程、文件系统、channel、ring、socket 和设备。失败返回不携带未定义的 `value0` 或 `value1`，避免用户程序依赖残留寄存器内容。

== 14.6 Capability handle 与对象模型

Native ABI 不把所有资源压缩为无类型文件描述符。每个 handle 都关联一个对象接口和一组不可扩大的权限。当前公开接口覆盖进程、地址空间、流、时钟、映像、事件端口、组件、线程、内存对象、目录、文件、channel、SubmissionRing、socket 和设备功能等对象。operation 注册表同时声明其目标接口和所需权限，dispatcher 在进入具体实现之前统一核对。

用户可见 handle 是 64 位值，高 32 位为 generation，低 32 位为从 1 开始的 slot index。关闭对象后，slot 的 generation 递增；旧 handle 再次使用会得到 `HANDLE_STALE`。generation 溢出的 slot 永久退役，避免经过完整回绕后把旧 handle 误认为新对象。`duplicate` 复制同一对象及其权限，`restrict` 只能生成权限子集，不能恢复已经移除的权限。

#figure(caption: figure-caption("图", "14-2", [Native operation 的能力检查顺序]))[
  #layer-card("Call Slot", [确认 slot 存在、绑定 operation，且调用参数符合该 operation 的固定形状], fill: soft-fill)
  #flow-arrow(label: "解析 handle")
  #layer-card("代际检查", [核对 index、generation 与占用状态，拒绝无效或陈旧 handle], fill: warm-fill)
  #flow-arrow(label: "核对授权")
  #layer-card("接口与权限", [对象接口必须匹配，operation 所需权限必须是 handle 权限的子集], fill: handoff-fill)
  #flow-arrow(label: "固定对象引用")
  #layer-card("执行 operation", [在持有对象引用而非 handle 表锁的条件下进入具体子系统], fill: stable-fill)
]

初始 capability 由 manifest requirement 与执行环境共同决定。例如，声明 `stdout` 并不意味着获得任意文件写权限，只会获得一个 `Stream` 接口的初始 handle，其权限不超过 manifest 请求的 `write` 等权限。创建子进程或发送 channel 消息时，handle 转移结构会再次声明目标 requirement、请求权限和复制或移动语义，从而使授权传播保持显式。

== 14.7 同步调用、SubmissionRing 与组件

同步 Native call 适合控制操作和短请求。对于批量 I/O，Native ABI 还提供 SubmissionRing。程序先注册 MemoryRegion，再提交固定大小 descriptor；内核把完成状态写入 completion queue。operation 注册表明确区分三种提交模式：只能同步调用、参数可完全内联、参数必须引用已注册 MemoryRegion。包含任意用户指针或进程控制状态的 operation 不能绕过同步路径进入 ring。

共享组件使用同一个 SOYO 容器，但采用独立的组件契约。组件由 128 位 component identity 和 ABI identity 标识；依赖项还可以绑定预期 content hash。接口导入与导出同时携带 interface identity、symbol identity 和签名哈希，避免仅凭字符串名称连接不同签名。装载事务会检查缺失依赖、版本冲突和依赖环，并在所有映像和绑定校验成功后统一激活。

组件生命周期依次经过 preparing、initializing、active、draining、finalizing 和 unloaded，失败则进入 failed。卸载先阻止新调用，再等待 active call 归零，最后执行终止入口并释放 TLS、接口 gate 和映射。接口 gate 保存组件、generation、调用状态和目标地址；每次进入接口都固定当前 generation，离开时归还活动调用计数，防止组件代码在栈上仍被执行时解除映射。

组件来源认证采用 Ed25519。签名消息包含固定域 `SOYO-SIGNATURE` 和映像 content hash，避免与其它签名协议混用。部署策略可以允许开发期 unsigned 映像，也可以配置可信公钥、撤销的 key ID 和拒绝回滚的 content hash。格式与摘要校验始终执行；允许 unsigned 只改变来源策略，不会关闭结构校验。

== 14.8 原生运行库与程序开发

C 程序通常链接 MRT 与 ranalib。MRT 提供入口、StartInfo 验证、Call Slot 封装、初始 handle 查询、构造与析构、进程和组件辅助函数。ranalib 提供适合 freestanding 环境的 C 标准库子集，包括堆、字符串、格式化 I/O、时间和线程接口。其标准输入输出不是 Linux 文件描述符，而是从 StartInfo 获得的 stream capability。堆通过当前地址空间 capability 调用 `memory.allocate` 和 `memory.free`。

Rust 程序使用 `no_std` 的 `anonlib`。它提供 channel、组件、设备、文件系统、内存、ring、socket 和线程等类型化封装，并复用 linker 从 manifest 生成的 Rust 绑定。C 与 Rust 运行时最终使用相同的 `NativeCall` 和 `NativeResult` 线布局，因此语言封装不会改变内核 ABI。

典型构建步骤如下：

1. 编写 `program.json`，只声明程序实际使用的 imports、capabilities 和 runtime 约束；
2. 使用 `soyo-ld --emit-c-header` 或 `--emit-rust-module` 从 manifest 生成绑定；
3. 用目标架构的 freestanding 参数把程序、MRT 和运行库编译为 little-endian ELF64 `ET_REL` 对象；
4. 使用同一 manifest 调用 `soyo-ld --target riscv64` 或 `--target loongarch64`，把对象直接链接为 `.soyo`；
5. 使用 `soyo-inspect` 检查格式、架构、ABI、段数、导入、能力、重定位和摘要，按部署策略使用 `soyo-verify` 验证签名；
6. 把产物安装到 rootfs 并直接执行。内核按 magic 选择 SOYO 装载路径，不需要文件扩展名参与判定。

#pseudo-sample("14-2", [构建并检查 RISC-V64 SOYO 程序], kind: "代码")[
  ```sh
  soyo-ld --target riscv64 --manifest program.json \
    --emit-c-header build/include/mygo_program.h

  clang --target=riscv64-unknown-none-elf -ffreestanding -fno-pic \
    -fno-pie -fno-stack-protector -mno-relax -msmall-data-limit=0 \
    -mcmodel=medany -Ibuild/include -Inative/include -c main.c -o main.o

  soyo-ld --target riscv64 --manifest program.json \
    -o app.soyo mrt.o main.o

  soyo-inspect app.soyo
  soyo-verify --allow-unsigned app.soyo
  ```
]

仓库中的 `native/examples` 给出了 C 与 Rust 的 hello、父子进程、动态组件、文件与 channel ring、socket ring、设备 ring 和组件仓库示例。顶层构建可通过 `NATIVE_EXAMPLES` 选择要装入 rootfs 的示例；启用 `soyo-tests` feature 时会安装默认原生集成场景。开发者应优先从与目标能力最接近的示例扩展，而不是手写生成头或硬编码 operation ID。

== 14.9 兼容边界与排障

MyGO Native ABI 与 Linux ABI 是并列 personality，不承诺二进制兼容。SOYO 程序不能直接链接 glibc 或 musl，也不能把 Native handle 当作 Linux fd；Linux ELF 程序同样不能直接使用 Call Slot。两条路径可以共享底层 VFS inode、socket、地址空间和调度对象，但必须经过各自的用户态契约投影。跨 personality 创建或替换进程时，内核重新构造完整的 personality payload，不复用另一套 ABI 的私有表。

装载失败首先按阶段定位：

- `soyo-inspect` 无法读取时，检查 magic、目录、表尺寸、对齐、填充、摘要和资源上限；
- 宿主检查通过而内核返回 `ENOEXEC` 时，检查目标架构、required feature、ABI epoch、operation 支持范围、签名策略和段布局；
- 程序进入后立即退出时，检查 MRT 的 StartInfo 校验、manifest 生成绑定是否与最终链接使用同一份文件，以及 required capability 是否实际授予；
- Native call 返回 ABI 类错误时，检查 Call Slot 与签名；返回 handle 或 security 类错误时，检查接口、generation 和权限；
- 阻塞调用异常时，检查外部进程控制、事件等待、ring completion 和对象生命周期，而不是按 Linux errno 推断。

当前实现支持 RISC-V64 与 LoongArch64、静态 TLS、构造与析构数组、动态组件、同步对象调用和 SubmissionRing。它没有把任意 ELF 动态库转换为组件，也没有承诺稳定支持 manifest 未登记的符号、未知 required feature 或跨 epoch 自动兼容。ABI 演进必须先更新机器注册表、生成绑定、宿主工具、内核策略和运行时验证，再提升 epoch；不能通过放宽旧解析器来掩盖不兼容变更。

== 14.10 工程设计总结

SOYO 把映像结构、ABI 导入、能力需求、运行时布局和来源身份放入一个可校验容器，MyGO Native ABI 则把用户态访问内核的方式约束为“Call Slot 加有类型的 capability handle”。前者解决映像能否安全装载，后者解决装载后能够调用什么、能够操作哪个对象以及拥有何种权限。

这套设计的核心不是增加一种文件扩展名，而是把传统执行环境中分散在动态符号、系统调用号、文件描述符和启动栈里的隐含契约改为显式机器数据。manifest、生成绑定、SOYO 元数据、StartInfo 和内核 operation registry 在不同阶段相互核对，使格式错误、ABI 不兼容和授权不足尽可能在改变系统状态之前被拒绝。同时，personality 分流让原生 ABI 可以独立演进，又不破坏现有 Linux 兼容用户态。

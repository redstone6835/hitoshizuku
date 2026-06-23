#import "../config.typ": appendix-title
#import "../styles/figure.typ": continued-table
#import "../styles/tokens.typ": handoff-fill, soft-fill, stable-fill, warm-fill

= #appendix-title

附录只放查阅型材料。正文各章讨论设计目标、机制边界和工程取舍，附录则保存与源码路径、接口范围和代码句柄直接相关的信息。这样处理可以避免正文被实现细节打断，也能给后续维护者留下较明确的阅读入口。

== A. 代码组织路径

表 附录-1 给出源码阅读时最常用的路径。这里列出的路径只用于帮助定位代码，不作为正文架构原则的替代说明。正文中的分层、信息流和子系统边界仍以各章论述为准。

#continued-table(
  "附录-1",
  [代码组织路径],
  (1.25fr, 2.25fr, 2.2fr),
  (
    table.cell(fill: soft-fill)[#text(weight: "bold")[路径]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[主要内容]],
    table.cell(fill: soft-fill)[#text(weight: "bold")[阅读场景]],
  ),
  (
    [`arch/`],
    [LoongArch64 与 RISC-V64 的入口、异常现场、页表细节、系统调用进入路径和平台指令封装。],
    [排查启动入口、异常处理、架构寄存器、系统调用快路径和页表格式时优先阅读。],

    [`hal/`],
    [把架构侧能力整理为上层可消费的运行期接口，包括用户上下文、调度钩子、用户访问和平台能力注册。],
    [需要理解平台相关能力如何交给通用子系统时阅读。],

    [`general/`],
    [启动上下文、固件视图、系统调用分发、用户地址空间、设备能力、进程间通信和跨子系统公共对象。],
    [阅读平台无关基础设施和跨子系统交接对象时使用。],

    [`kernel/`],
    [内核主入口、系统调用实现、用户程序装载、信号兼容、VFS 适配、设备挂接和运行期编排。],
    [追踪用户态请求如何进入具体内核策略时阅读。],

    [`libs/allocator/`],
    [物理页、Slab 分配器、内核堆和受管堆相关的底层分配能力。],
    [排查内存分配、对象回收和分配失败路径时阅读。],

    [`libs/vfs/`],
    [索引节点、目录项、文件对象、文件描述符表、挂载命名空间、设备文件和套接字文件适配。],
    [追踪文件系统、设备文件、路径解析和文件描述符生命周期时阅读。],

    [`libs/sched/`],
    [任务对象、调度类、运行队列、等待队列、信号状态、凭据、资源限制和任务生命周期。],
    [排查调度、阻塞唤醒、进程关系和信号状态时阅读。],

    [`libs/net/`],
    [网络设备对象、网络栈、接口管理、路由、协议栈适配和网络套接字句柄。],
    [排查网络设备接入、协议推进、套接字状态和路由行为时阅读。],

    [`libs/socket/`],
    [Unix 套接字、内核内存队列、套接字状态和本机进程间通信对象。],
    [排查 Unix 套接字和非网络协议栈的数据路径时阅读。],

    [`libs/errno/`],
    [统一错误码枚举以及错误码和整数返回值之间的转换。],
    [核对系统调用错误返回、兼容层错误码和子系统错误映射时阅读。],

    [`userland/`],
    [用户态根文件系统、辅助脚本、场景文件和随镜像进入用户态的程序资源。],
    [需要确认用户态程序、启动脚本和镜像内容时阅读。],
  ),
  kind: "表",
  continuation-kind: "续表",
  align: (left, left, left),
)

== B. 系统调用和 errno 表

系统调用实现分散在 `kernel/src/syscalls/` 及其调用的通用子系统中。表 附录-2 以 C 语言函数原型的形式列出当前文档中涉及的主要系统调用入口。这里的原型用于说明用户态 ABI 形态，具体参数布局仍以目标架构的系统调用约定和内核实现为准。

#continued-table(
  "附录-2",
  [系统调用函数原型],
  (2.55fr, 3fr),
  (
    table.cell(fill: warm-fill)[#text(weight: "bold")[syscall 函数原型]],
    table.cell(fill: warm-fill)[#text(weight: "bold")[作用]],
  ),
  (
    [`pid_t fork(void);`], [复制当前进程，子进程获得独立任务身份和写时复制地址空间。],
    [`pid_t vfork(void);`], [创建临时共享地址空间的子进程，通常用于立即执行替换。],
    [`long clone(unsigned long flags, void *stack, int *parent_tid, int *child_tid, unsigned long tls);`], [按照共享标志创建线程或进程，是进程和线程创建的底层入口。],
    [`int execve(const char *pathname, char *const argv[], char *const envp[]);`], [用新用户程序替换当前进程映像。],
    [`void _exit(int status);`], [终止当前进程或线程组并进入退出回收流程。],
    [`pid_t wait4(pid_t pid, int *wstatus, int options, struct rusage *rusage);`], [等待子进程状态变化并回收僵尸任务。],
    [`pid_t getpid(void);`], [返回当前进程标识符。],
    [`pid_t gettid(void);`], [返回当前线程标识符。],
    [`int setuid(uid_t uid);`], [修改当前任务的用户身份。],
    [`int setgid(gid_t gid);`], [修改当前任务的组身份。],

    [`int openat(int dirfd, const char *pathname, int flags, mode_t mode);`], [相对目录文件描述符打开路径并创建文件对象。],
    [`int close(int fd);`], [关闭文件描述符并释放对文件对象的引用。],
    [`ssize_t read(int fd, void *buf, size_t count);`], [从文件对象读取数据到用户缓冲区。],
    [`ssize_t write(int fd, const void *buf, size_t count);`], [把用户缓冲区数据写入文件对象。],
    [`ssize_t pread64(int fd, void *buf, size_t count, off_t offset);`], [在指定文件偏移处读取，不改变共享文件偏移。],
    [`ssize_t pwrite64(int fd, const void *buf, size_t count, off_t offset);`], [在指定文件偏移处写入，不改变共享文件偏移。],
    [`int statx(int dirfd, const char *pathname, int flags, unsigned int mask, struct statx *buf);`], [查询路径或文件对象的扩展元数据。],
    [`ssize_t getdents64(unsigned int fd, struct linux_dirent64 *dirp, unsigned int count);`], [读取目录项序列。],
    [`int renameat2(int olddirfd, const char *oldpath, int newdirfd, const char *newpath, unsigned int flags);`], [重命名路径，必要时处理替换、交换或禁止覆盖语义。],
    [`int unlinkat(int dirfd, const char *pathname, int flags);`], [删除目录项或目录。],

    [`int fcntl(int fd, int cmd, ...);`], [读取或修改文件描述符标志、文件状态标志和相关控制状态。],
    [`int dup(int oldfd);`], [复制文件描述符到新的最小可用编号。],
    [`int dup3(int oldfd, int newfd, int flags);`], [复制文件描述符到指定编号并设置执行时关闭等标志。],
    [`int close_range(unsigned int first, unsigned int last, unsigned int flags);`], [批量关闭或修改一段文件描述符。],
    [`int ioctl(int fd, unsigned long request, ...);`], [向设备、终端或特殊文件对象发送控制命令。],
    [`int poll(struct pollfd *fds, nfds_t nfds, int timeout);`], [等待一组文件描述符的就绪事件。],
    [`int ppoll(struct pollfd *fds, nfds_t nfds, const struct timespec *timeout, const sigset_t *sigmask);`], [带信号掩码切换的文件描述符就绪等待。],
    [`int epoll_create1(int flags);`], [创建可扩展事件轮询实例。],
    [`int epoll_ctl(int epfd, int op, int fd, struct epoll_event *event);`], [向事件轮询实例添加、修改或删除监听目标。],
    [`int epoll_wait(int epfd, struct epoll_event *events, int maxevents, int timeout);`], [等待事件轮询实例返回已就绪事件。],

    [`int brk(void *addr);`], [调整传统用户堆顶位置。],
    [`void *mmap(void *addr, size_t length, int prot, int flags, int fd, off_t offset);`], [建立匿名或文件后备的用户虚拟内存映射。],
    [`int munmap(void *addr, size_t length);`], [解除用户虚拟内存映射。],
    [`int mprotect(void *addr, size_t len, int prot);`], [修改用户虚拟内存区域权限。],
    [`void *mremap(void *old_address, size_t old_size, size_t new_size, int flags, ...);`], [调整既有映射范围，必要时移动映射。],
    [`int mlock(const void *addr, size_t len);`], [请求锁定一段用户内存范围。],
    [`void *shmat(int shmid, const void *shmaddr, int shmflg);`], [把系统 V 共享内存段挂接到当前地址空间。],
    [`int shmdt(const void *shmaddr);`], [从当前地址空间分离系统 V 共享内存段。],
    [`int shmctl(int shmid, int cmd, struct shmid_ds *buf);`], [查询、修改或删除系统 V 共享内存段。],

    [`int rt_sigaction(int signum, const struct sigaction *act, struct sigaction *oldact, size_t sigsetsize);`], [安装或读取信号处理动作。],
    [`int rt_sigprocmask(int how, const sigset_t *set, sigset_t *oldset, size_t sigsetsize);`], [修改或读取当前线程信号屏蔽字。],
    [`int rt_sigreturn(void);`], [从用户信号帧恢复信号发生前的陷阱帧。],
    [`int rt_sigsuspend(const sigset_t *mask, size_t sigsetsize);`], [临时替换信号屏蔽字并等待信号。],
    [`int rt_sigtimedwait(const sigset_t *set, siginfo_t *info, const struct timespec *timeout, size_t sigsetsize);`], [限时等待指定信号集合中的信号。],
    [`int kill(pid_t pid, int sig);`], [向进程或进程组发送信号。],
    [`int tgkill(int tgid, int tid, int sig);`], [向指定线程组中的指定线程发送信号。],
    [`int pidfd_send_signal(int pidfd, int sig, siginfo_t *info, unsigned int flags);`], [通过 pid 文件描述符发送信号。],

    [`int clock_gettime(clockid_t clk_id, struct timespec *tp);`], [读取指定时钟的当前时间。],
    [`int gettimeofday(struct timeval *tv, struct timezone *tz);`], [读取墙钟时间。],
    [`int nanosleep(const struct timespec *req, struct timespec *rem);`], [让当前任务睡眠指定时间。],
    [`int sched_yield(void);`], [当前任务主动让出处理器。],
    [`int getrusage(int who, struct rusage *usage);`], [读取任务或子任务资源用量。],
    [`int getrlimit(int resource, struct rlimit *rlim);`], [读取资源限制。],
    [`int setrlimit(int resource, const struct rlimit *rlim);`], [设置资源限制。],

    [`int socket(int domain, int type, int protocol);`], [创建网络套接字或本地套接字文件描述符。],
    [`int bind(int sockfd, const struct sockaddr *addr, socklen_t addrlen);`], [绑定套接字本地地址。],
    [`int listen(int sockfd, int backlog);`], [把流式套接字切换为监听状态。],
    [`int accept4(int sockfd, struct sockaddr *addr, socklen_t *addrlen, int flags);`], [接受监听队列中的连接并返回新套接字。],
    [`int connect(int sockfd, const struct sockaddr *addr, socklen_t addrlen);`], [发起连接或设置默认远端地址。],
    [`ssize_t sendto(int sockfd, const void *buf, size_t len, int flags, const struct sockaddr *dest_addr, socklen_t addrlen);`], [向套接字发送数据，必要时指定远端地址。],
    [`ssize_t sendmsg(int sockfd, const struct msghdr *msg, int flags);`], [按消息头描述发送分散缓冲区和控制信息。],
    [`ssize_t recvfrom(int sockfd, void *buf, size_t len, int flags, struct sockaddr *src_addr, socklen_t *addrlen);`], [从套接字接收数据并返回来源地址。],
    [`ssize_t recvmsg(int sockfd, struct msghdr *msg, int flags);`], [按消息头描述接收数据和控制信息。],
    [`int shutdown(int sockfd, int how);`], [关闭套接字的读端、写端或双向通信。],

    [`int futex(uint32_t *uaddr, int op, uint32_t val, const struct timespec *timeout, uint32_t *uaddr2, uint32_t val3);`], [围绕用户地址上的整数执行等待、唤醒和同步操作。],
    [`int eventfd(unsigned int initval, int flags);`], [创建事件计数文件描述符。],
    [`int pipe2(int pipefd[2], int flags);`], [创建管道读写端文件描述符。],
    [`int timerfd_create(int clockid, int flags);`], [创建定时器文件描述符。],
    [`int timerfd_settime(int fd, int flags, const struct itimerspec *new_value, struct itimerspec *old_value);`], [设置定时器文件描述符的到期时间和周期。],
  ),
  kind: "表",
  continuation-kind: "续表",
  align: (left, left),
)

表 附录-3 整理 POSIX `errno` 名称。该表列名称和语义，不列数值，因为具体整数值可能受平台 ABI 影响。当前内核是否已经完整产生这些错误码，仍以 `libs/errno/src/lib.rs` 和具体系统调用实现为准。

#continued-table(
  "附录-3",
  [POSIX errno 表],
  (1.15fr, 1.65fr, 2.65fr),
  (
    table.cell(fill: handoff-fill)[#text(weight: "bold")[错误码]],
    table.cell(fill: handoff-fill)[#text(weight: "bold")[POSIX 语义]],
    table.cell(fill: handoff-fill)[#text(weight: "bold")[中文说明]],
  ),
  (
    [`E2BIG`],
    [Argument list too long],
    [参数列表过长。],

    [`EACCES`],
    [Permission denied],
    [权限检查拒绝访问。],

    [`EADDRINUSE`],
    [Address in use],
    [地址或端口已经被占用。],

    [`EADDRNOTAVAIL`],
    [Address not available],
    [请求的本地地址不可用。],

    [`EAFNOSUPPORT`],
    [Address family not supported],
    [地址族不被支持。],

    [`EAGAIN`],
    [Resource unavailable, try again],
    [资源暂时不可用，调用方可以稍后重试。],

    [`EALREADY`],
    [Connection already in progress],
    [连接操作已经在进行中。],

    [`EBADF`],
    [Bad file descriptor],
    [文件描述符无效。],

    [`EBADMSG`],
    [Bad message],
    [消息格式或内容无效。],

    [`EBUSY`],
    [Device or resource busy],
    [设备或资源正忙。],

    [`ECANCELED`],
    [Operation canceled],
    [操作被取消。],

    [`ECHILD`],
    [No child processes],
    [没有可等待的子进程。],

    [`ECONNABORTED`],
    [Connection aborted],
    [连接被中止。],

    [`ECONNREFUSED`],
    [Connection refused],
    [连接请求被拒绝。],

    [`ECONNRESET`],
    [Connection reset],
    [连接被重置。],

    [`EDEADLK`],
    [Resource deadlock would occur],
    [操作会导致资源死锁。],

    [`EDESTADDRREQ`],
    [Destination address required],
    [需要提供目标地址。],

    [`EDOM`],
    [Mathematics argument out of domain of function],
    [数学函数参数超出定义域。],

    [`EDQUOT`],
    [Reserved],
    [POSIX 保留错误名，传统语义通常表示磁盘配额耗尽。],

    [`EEXIST`],
    [File exists],
    [目标文件、目录或命名对象已存在。],

    [`EFAULT`],
    [Bad address],
    [用户地址或指针无效。],

    [`EFBIG`],
    [File too large],
    [文件过大。],

    [`EHOSTUNREACH`],
    [Host is unreachable],
    [目标主机不可达。],

    [`EIDRM`],
    [Identifier removed],
    [IPC 标识符已经被移除。],

    [`EILSEQ`],
    [Illegal byte sequence],
    [非法字节序列。],

    [`EINPROGRESS`],
    [Operation in progress],
    [操作正在进行中。],

    [`EINTR`],
    [Interrupted function],
    [函数调用被信号中断。],

    [`EINVAL`],
    [Invalid argument],
    [参数无效。],

    [`EIO`],
    [I/O error],
    [输入输出错误。],

    [`EISCONN`],
    [Socket is connected],
    [套接字已经连接。],

    [`EISDIR`],
    [Is a directory],
    [目标是目录。],

    [`ELOOP`],
    [Too many levels of symbolic links],
    [符号链接层级过多。],

    [`EMFILE`],
    [File descriptor value too large],
    [进程打开的文件描述符数量达到限制。],

    [`EMLINK`],
    [Too many links],
    [链接数过多。],

    [`EMSGSIZE`],
    [Message too large],
    [消息过大。],

    [`EMULTIHOP`],
    [Reserved],
    [POSIX 保留错误名，传统语义通常表示多跳链路错误。],

    [`ENAMETOOLONG`],
    [Filename too long],
    [文件名或路径名过长。],

    [`ENETDOWN`],
    [Network is down],
    [网络不可用。],

    [`ENETRESET`],
    [Connection aborted by network],
    [连接被网络侧中止。],

    [`ENETUNREACH`],
    [Network unreachable],
    [网络不可达。],

    [`ENFILE`],
    [Too many files open in system],
    [系统范围内打开文件数量达到限制。],

    [`ENOBUFS`],
    [No buffer space available],
    [缓冲区空间不足。],

    [`ENODATA`],
    [No message is available on the stream head read queue],
    [流首读队列中没有可用消息。],

    [`ENODEV`],
    [No such device],
    [没有对应设备。],

    [`ENOENT`],
    [No such file or directory],
    [没有对应文件或目录。],

    [`ENOEXEC`],
    [Executable file format error],
    [可执行文件格式错误。],

    [`ENOLCK`],
    [No locks available],
    [没有可用锁。],

    [`ENOLINK`],
    [Reserved],
    [POSIX 保留错误名，传统语义通常表示链路断开。],

    [`ENOMEM`],
    [Not enough space],
    [内存或地址空间不足。],

    [`ENOMSG`],
    [No message of the desired type],
    [没有所需类型的消息。],

    [`ENOPROTOOPT`],
    [Protocol not available],
    [协议选项不可用。],

    [`ENOSPC`],
    [No space left on device],
    [设备或文件系统空间不足。],

    [`ENOSR`],
    [No stream resources],
    [流资源不足。],

    [`ENOSTR`],
    [Not a stream],
    [对象不是流。],

    [`ENOSYS`],
    [Functionality not supported],
    [功能不被支持。],

    [`ENOTCONN`],
    [The socket is not connected],
    [套接字尚未连接。],

    [`ENOTDIR`],
    [Not a directory],
    [路径组件不是目录。],

    [`ENOTEMPTY`],
    [Directory not empty],
    [目录非空。],

    [`ENOTRECOVERABLE`],
    [State not recoverable],
    [互斥量保护状态不可恢复。],

    [`ENOTSOCK`],
    [Not a socket],
    [对象不是套接字。],

    [`ENOTSUP`],
    [Not supported],
    [操作不被支持。],

    [`ENOTTY`],
    [Inappropriate I/O control operation],
    [不适合的输入输出控制操作。],

    [`ENXIO`],
    [No such device or address],
    [没有对应设备或地址。],

    [`EOPNOTSUPP`],
    [Operation not supported on socket],
    [套接字不支持该操作。],

    [`EOVERFLOW`],
    [Value too large to be stored in data type],
    [数值过大，无法存入目标数据类型。],

    [`EOWNERDEAD`],
    [Previous owner died],
    [健壮互斥量的前一个拥有者已经终止。],

    [`EPERM`],
    [Operation not permitted],
    [操作不被允许。],

    [`EPIPE`],
    [Broken pipe],
    [管道或套接字对端已经关闭。],

    [`EPROTO`],
    [Protocol error],
    [协议错误。],

    [`EPROTONOSUPPORT`],
    [Protocol not supported],
    [协议不被支持。],

    [`EPROTOTYPE`],
    [Protocol wrong type for socket],
    [套接字协议类型不匹配。],

    [`ERANGE`],
    [Result too large],
    [结果超出可表示范围。],

    [`EROFS`],
    [Read-only file system],
    [只读文件系统。],

    [`ESPIPE`],
    [Invalid seek],
    [无效定位操作。],

    [`ESRCH`],
    [No such process],
    [没有对应进程。],

    [`ESTALE`],
    [Reserved],
    [POSIX 保留错误名，传统语义通常表示陈旧文件句柄。],

    [`ETIME`],
    [Stream ioctl() timeout],
    [流控制操作超时。],

    [`ETIMEDOUT`],
    [Connection timed out],
    [连接超时。],

    [`ETXTBSY`],
    [Text file busy],
    [文本文件正忙。],

    [`EWOULDBLOCK`],
    [Operation would block],
    [操作会阻塞。该错误码可以与 `EAGAIN` 取相同值。],

    [`EXDEV`],
    [Cross-device link],
    [跨设备链接或重命名。],
  ),
  kind: "表",
  continuation-kind: "续表",
  align: (left, left, left),
)

== C. 核心概念与代码句柄对照

正文优先使用中文概念。需要回到源码时，表 附录-4 可以作为概念和代码句柄之间的索引。表中列出的代码句柄均应在正文中使用反引号表示。

#continued-table(
  "附录-4",
  [核心概念与代码句柄对照],
  (1.25fr, 1.55fr, 2.4fr),
  (
    table.cell(fill: stable-fill)[#text(weight: "bold")[中文概念]],
    table.cell(fill: stable-fill)[#text(weight: "bold")[代码句柄]],
    table.cell(fill: stable-fill)[#text(weight: "bold")[主要位置]],
  ),
  (
    [启动上下文],
    [`StartContext`],
    [`general/src/start.rs`。启动阶段整理固件、内存、命令行和架构交接信息。],

    [系统调用上下文],
    [`SyscallContext`],
    [`general/src/syscall.rs`。系统调用分发时保存任务、参数、返回值和陷阱帧访问入口。],

    [陷阱帧],
    [`TrapFrame`],
    [`arch/src/riscv64/trap_frame.rs` 与 `arch/src/loongarch64/specific.rs`。保存用户态寄存器和异常现场。],

    [设备能力],
    [`DeviceFunction`],
    [`general/src/dev/function.rs`。设备向内核发布的统一能力边界。],

    [字符设备对象],
    [`CharDevice`],
    [`general/src/dev/char.rs`。字符流设备和终端后端的类型化对象。],

    [块设备对象],
    [`BlockDevice`],
    [`general/src/dev/block.rs`。块请求提交、同步等待和设备几何信息的类型化对象。],

    [网络设备对象],
    [`NetDevice`],
    [`libs/net/src/device.rs`。网络接口身份、链路状态、MTU 和收发统计的承载对象。],

    [实时时钟设备],
    [`RtcDevice`],
    [`general/src/dev/rtc.rs`。时间读写、告警和周期中断相关能力。],

    [文件操作接口],
    [`FileOps`],
    [`libs/vfs/src/file.rs`。读写、控制、轮询和关闭等文件行为入口。],

    [文件描述符表],
    [`FdTable`],
    [`libs/vfs/src/fdtable.rs`。文件描述符到文件对象的映射和执行时关闭标志管理。],

    [用户虚拟地址空间],
    [`VmSpace`],
    [`general/src/mm/vm_space.rs`。VMA、缺页、映射、权限变更和执行装载相关地址空间操作。],

    [任务对象],
    [`Task`],
    [`libs/sched/src/task.rs`。任务身份、调度状态、信号状态、凭据和资源视图。],

    [网络栈],
    [`NetStack`],
    [`libs/net/src/stack.rs`。接口注册表、路由、协议栈推进和网络套接字管理。],

    [网络套接字状态],
    [`SocketState`],
    [`libs/net/src/socket.rs`。网络套接字连接、监听、收发和关闭状态。],

    [Unix 套接字对象],
    [`Socket`],
    [`libs/socket/src/state.rs`。本机 Unix 套接字的连接和内存队列状态。],

    [统一错误码],
    [`Errno`],
    [`libs/errno/src/lib.rs`。内核内部错误和用户态负 errno 返回值之间的统一枚举。],
  ),
  kind: "表",
  continuation-kind: "续表",
  align: (left, left, left),
)

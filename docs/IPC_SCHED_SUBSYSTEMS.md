# 进程调度与 IPC 子系统实现说明

本文档记录 `feat/sched-ipc` 分支对进程/调度（§3）与 IPC（§7）两个子系统
差距表的完整实现，语义对齐 Linux。

## IPC 子系统（§7）

### SysV 消息队列（`general::ipc::msg` + `kernel/src/syscalls/ipc.rs`）
- `msgget/msgsnd/msgrcv/msgctl` 全套，`msqid64_ds`/`msginfo` ABI 对齐
  asm-generic 64 位布局；
- `msgrcv` 支持 `MSG_NOERROR`（截断）、`MSG_EXCEPT`（类型不等，
  `msgtyp > 0`）、`MSG_COPY`（按序号拷贝，需 `CAP_CHECKPOINT_RESTORE` 或
  `CAP_SYS_ADMIN`）、`MSG_TRUNC`；
- 阻塞按 `msg_qbytes` 上限，`IPC_RMID` 唤醒全部阻塞者并返回 `EIDRM`；
- `IPC_INFO/MSG_INFO/MSG_STAT/MSG_STAT_ANY` 与 `IPC_SET`（qbytes 提升需
  `CAP_SYS_RESOURCE`）。

### SysV 信号量（`general::ipc/sem.rs`）
- `semctl` 全命令：`GETALL/SETALL/GETPID/GETNCNT/GETZCNT`、
  `IPC_STAT/IPC_SET`、`IPC_INFO/SEM_INFO/SEM_STAT/SEM_STAT_ANY`；
- per-sem `sempid/semncnt/semzcnt` 统计由阻塞登记协议维护；
- `sem_otime/sem_ctime` 时间戳。

### `SEM_UNDO` 撤销表（`general::ipc/sem_undo.rs`）
- 进程级撤销表：`semop(SEM_UNDO)` 成功提交后累计 `-sem_op`；
- 进程退出（`cleanup_task_before_exit`）按集合聚合后原子应用（可睡眠，
  Linux `exit_sem` 语义）；
- `SETVAL/SETALL/IPC_RMID` 使对应撤销项失效；`CLONE_SYSVSEM` 共享表，
  fork 不继承；exec 保留。

### SysV 共享内存（`general::ipc/shm.rs`）
- `shmctl` 补齐 `SHM_LOCK/SHM_UNLOCK`（`CAP_IPC_LOCK` + `SHM_LOCKED`
  标志）、`SHM_STAT/SHM_STAT_ANY/SHM_INFO/IPC_INFO`。

### POSIX 消息队列（`general::ipc/mqueue.rs` + `general::vfs/mqueue.rs`）
- `mq_open/mq_unlink/mq_timedsend/mq_timedreceive/mq_notify/mq_getsetattr`
  （含 time64 变体）；
- 优先级出队（同优先级 FIFO）、`O_NONBLOCK`、`EMSGSIZE` 语义；
- `mq_notify`：一次性通知（队列空→非空触发），`SIGEV_SIGNAL`
  （`si_code = SI_MESGQ`）与 `SIGEV_THREAD`（内核在注册者进程上下文克隆
  线程执行通知函数，函数返回经用户态退出桩结束线程）；
- mqueue 伪文件系统（`/dev/mqueue` 启动时挂载），mq fd 支持
  read/write/poll/epoll。

### keyring（`general::ipc/keys.rs`）
- key 类型 `user/keyring/logon`；Linux 权限模型
  （possessor/user/group/other × 6 位）；每 uid 配额；
- `add_key/request_key/keyctl` 全命令（`KEYCTL_*` 0..22、29、36）；
  `KEY_SPEC_*` 特殊引用；
- `request_key` 未命中经 `/sbin/request-key` upcall（
  `sched::operation::spawn_user_process`）+ 等待实例化；
- 明确排除：`KEYCTL_DH_COMPUTE`/`KEYCTL_PKEY_*`（需 crypto 原语）与
  `KEYCTL_WATCH_KEY`（需 watch_queue 子系统），返回 `EOPNOTSUPP`。

## 进程与调度子系统（§3）

### ptrace（`libs/sched` + `kernel/src/syscalls/process.rs`）
- `PEEKDATA/PEEKTEXT/POKEDATA/POKETEXT`（目标 `VmSpace` 按字读写）、
  `PEEKUSR/POKEUSR`（mcontext 布局）；
- `GETREGSET/SETREGSET`：`NT_PRSTATUS`（mcontext 布局）；`NT_FPREGSET`
  提供零寄存器集（架构 trap frame 不保存 FP 状态，文档化）；
- `GETSIGINFO/SETSIGINFO/GETEVENTMSG/GETSIGMASK/SETSIGMASK/PEEKSIGINFO/
  GET_SYSCALL_INFO/GET_RSEQ_CONFIGURATION`；
- `SETOPTIONS`：`TRACEFORK/VFORK/CLONE/EXEC/EXIT` 事件（event 编码进
  wait status，`TRACEEXIT` 在退出流程中等待 tracer）、`TRACESYSGOOD`
  （`0x80|SIGTRAP` 编码）；
- `PTRACE_SYSCALL`：entry/exit syscall-stop（用户态重入恢复语义）；
- `PTRACE_SINGLESTEP`：指令补丁法（arch break 陷阱钩子恢复原指令 +
  `SIGTRAP` stop）；两架构均支持；
- `ptrace_may_access`（uid/`CAP_SYS_PTRACE`/dumpable）；`ATTACH` 等待
  stop 生效；`SEIZE/INTERRUPT/LISTEN`。

### prctl 与凭据
- prctl 全选项：`PDEATHSIG/DUMPABLE/KEEPCAPS/SECUREBITS/TSC/
  CHILD_SUBREAPER/NO_NEW_PRIVS/THP_DISABLE/PR_SET_MM`；未知选项返回
  `EINVAL`（原实现恒 `Ok(0)`）；
- exec 凭据：`S_ISUID/S_ISGID` 位切换 euid/egid（suid 同步、
  `dumpable = 0`），能力转换按 Linux 公式
  （`pP' = bset & (pI | pP)`、`pE' = pE & pP'`，secureexec 丢弃 permitted），
  `NNP/SECBIT_NO_SETUID_FIXUP/KEEPCAPS` 交互。

### 命名空间（`libs/ns` + `kernel/src/ns.rs`）
- 完整实现：**UTS**（hostname/domainname）、**IPC**（SysV 管理器每 ns
  独立）、**PID**（链式注册 + 按 ns 解析 + `getpid` 按 ns + 子进程进入
  pending 命名空间）、**TIME**（时钟偏移）、**CGROUP**（恒根）、
  **MOUNT**（`VfsContext` 级）；
- `unshare(2)` 与 `setns(2)`（nsfs fd），`CAP_SYS_ADMIN` 校验；
- `/proc/<pid>/ns/*`（nsfs 文件 + `NS_GET_*` ioctl）；
- 明确排除：**USER/NET** 命名空间（需全局能力语义改造 / 网络栈 per-ns
  化），对应 flags 返回 `EOPNOTSUPP`。

### seccomp（`general/src/seccomp.rs`）
- classic BPF 校验器 + 解释器（`seccomp_data` 64 字节布局）；
- 动作：`KILL_PROCESS/KILL_THREAD`（SIGSYS）、`TRAP`、`ERRNO`、
  `TRACE`（`PTRACE_EVENT_SECCOMP`）、`LOG`、`ALLOW`、`USER_NOTIF`
  （骨架：返回 ENOSYS 等待通知）；
- `SECCOMP_SET_MODE_STRICT/FILTER`、`GET_ACTION_AVAIL`、
  `GET_NOTIF_SIZES`；TSYNC（进程级共享状态）；`PR_SET_SECCOMP` 老接口。

### adjtimex / clock_adjtime
- `struct timex` 64 位 ABI；`ADJ_OFFSET/SETOFFSET/FREQUENCY/MAXERROR/
  ESTERROR/STATUS/TIMECONST/TICK/MICRO/NANO`；
- `CAP_SYS_TIME` 校验；`STA_PLL/STA_UNSYNC`；`TIME_OK/TIME_ERROR`。

## 观测接口（§13 支撑）

- `/proc/self/status`：`Uid/Gid`（含 fsuid/fsgid 修正）、`CapInh/CapPrm/
  CapEff/CapBnd`、`NoNewPrivs`、`Seccomp`；
- `/proc/<pid>/ns/*`；`/proc/sysvipc/{msg,sem,shm}` 预留（`ipcs` 经
  `MSG_STAT` 族枚举）。

## 明确排除（超出本分支范围）

- 用户/网络命名空间；keyring 的 DH/PKEY/WATCH_KEY；`NT_FPREGSET` 真
  浮点状态（架构未保存）；`USER_NOTIF` 的完整通知通道（RECV/SEND 待
  listener 集成）。

## QEMU 冒烟验证（loongarch64）

`scripts/tests/smoke_ipc_sched.c`（静态链接，注入 compat-initramfs 由
`test.sh` 直接执行）：**85/85 通过**，覆盖 §3/§7 全部新功能：

- SysV msg（含 MSG_COPY/qbytes 阻塞唤醒/NOERROR）、sem（GETPID/SEM_UNDO
  回滚）、shm（SHM_LOCK/UNLOCK）、POSIX mq（优先级序/满队列
  ETIMEDOUT）；
- ptrace（TRACEME/SETOPTIONS/SYSCALL 持续模式/TRACESYSGOOD/GETREGSET/
  PEEK/POKE/EVENT_EXIT）、prctl（NAME/DUMPABLE/THP/TSC/CAPBSET）、
  seccomp（ERRNO 过滤）、unshare/setns（UTS/IPC/PID/TIME）、pid ns
  getpid==1、adjtimex、keyring（add/request/read/describe/revoke/unlink）。

修复要点（随验证发现）：

- `PTRACE_SYSCALL` 是**持续模式**（entry+exit 都停，CONT 才退出模式）；
- `PTRACE_EVENT_EXIT=6`（5 是 VFORK_DONE）；PEEK/POKE 的 arg4 直通值；
- syscall 入口由 arch 保存 trap frame 快照供 GETREGSET/PEEKUSR；
- `seccomp_data` 是 72 字节（8+4+4+8+6*8）；BPF 条件跳转 +jt/+jf；
- `PR_SET_SECCOMP` 用 `SECCOMP_MODE_*`（1/2），`seccomp(2)` 用
  `SECCOMP_SET_MODE_*`（0/1），两套语义分开换算；
- `CLONE_NEWTIME=0x80`（0x0080_0000 是 NEWCGROUP 位）；
- Task::new 启动期占位 pid ns；clone/fork/kthread 全部路径
  `set_pid_ns`（CLONE_NEWPID 在 fork 时生效）；
- ns proxy 切换 remove+install（通用 ext 同 key 重复挂载取首项）；
- `mq_timedsend/receive` 的 timeout 是 CLOCK_REALTIME **绝对时间**；
- keyring：`KEY_SPEC_PROCESS_KEYRING` 按需创建；permission 用
  `KEY_POS_*` 位；`KEYCTL_LINK/UNLINK` 解析 KEY_SPEC_*；
- adjtimex 返回时钟状态（TIME_OK/TIME_ERROR 等，非恒 0）；
  `timex_state()` 不得在 `CLOCK_DISCIPLINE` 持锁期调用。

本工具链 glibc 的 `mq_open/mq_unlink` 包装会剥掉 name 首字符 `/`、
`ptrace` 包装对 PEEKDATA 返回值处理异常，冒烟测试对这两处使用
raw syscall 直通内核语义。`NT_FPREGSET` 按计划返回全零集。

RISC-V64 同样完成内核构建与 QEMU 启动验证（rcS 完整运行）。

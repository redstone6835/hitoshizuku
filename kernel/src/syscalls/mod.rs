//! Kernel 层具体 syscall 实现 + 启动期填表。
//!
//! 分文件按 syscall 类别组织：process / fs / mm / signal。每个 fn 签名统一
//! 为 `fn(&mut SyscallContext) -> Result<usize, Errno>`，由 general 层的
//! 分发主循环调用。号码常量集中在 `nr.rs`。
//!
//! `register_all()` 在 `kernel::sched::boot_init` 的末尾调用一次。

mod fs;
mod ipc;
mod mm;
mod nr;
mod process;
mod signal;
mod syslog;

use general::syscall::register_syscall;

pub fn register_all() {
    // 文件 I/O
    register_syscall(nr::SYS_GETCWD, fs::sys_getcwd);
    register_syscall(nr::SYS_DUP, fs::sys_dup);
    register_syscall(nr::SYS_DUP3, fs::sys_dup3);
    register_syscall(nr::SYS_FCNTL, fs::sys_fcntl);
    register_syscall(nr::SYS_IOCTL, fs::sys_ioctl);
    register_syscall(nr::SYS_PIPE2, fs::sys_pipe2);
    register_syscall(nr::SYS_MKDIRAT, fs::sys_mkdirat);
    register_syscall(nr::SYS_UNLINKAT, fs::sys_unlinkat);
    register_syscall(nr::SYS_RENAMEAT2, fs::sys_renameat2);
    register_syscall(nr::SYS_LINKAT, fs::sys_linkat);
    register_syscall(nr::SYS_SYMLINKAT, fs::sys_symlinkat);
    register_syscall(nr::SYS_MKNODAT, fs::sys_mknodat);
    register_syscall(nr::SYS_FCHMODAT, fs::sys_fchmodat);
    register_syscall(nr::SYS_FCHOWNAT, fs::sys_fchownat);
    register_syscall(nr::SYS_UTIMENSAT, fs::sys_utimensat);
    register_syscall(nr::SYS_TRUNCATE, fs::sys_truncate);
    register_syscall(nr::SYS_FTRUNCATE, fs::sys_ftruncate);
    register_syscall(nr::SYS_FSYNC, fs::sys_fsync);
    register_syscall(nr::SYS_FDATASYNC, fs::sys_fdatasync);
    register_syscall(nr::SYS_GETDENTS64, fs::sys_getdents64);
    register_syscall(nr::SYS_STATFS, fs::sys_statfs);
    register_syscall(nr::SYS_FSTATFS, fs::sys_fstatfs);
    register_syscall(nr::SYS_CHDIR, fs::sys_chdir);
    register_syscall(nr::SYS_FCHDIR, fs::sys_fchdir);
    register_syscall(nr::SYS_CHROOT, fs::sys_chroot);
    register_syscall(nr::SYS_MOUNT, fs::sys_mount);
    register_syscall(nr::SYS_PIVOT_ROOT, fs::sys_pivot_root);
    register_syscall(nr::SYS_UMOUNT2, fs::sys_umount2);
    register_syscall(nr::SYS_SYNC, fs::sys_sync);
    register_syscall(nr::SYS_SYNCFS, fs::sys_syncfs);
    register_syscall(nr::SYS_SENDFILE, fs::sys_sendfile);
    register_syscall(nr::SYS_COPY_FILE_RANGE, fs::sys_copy_file_range);
    register_syscall(nr::SYS_FALLOCATE, fs::sys_fallocate);
    register_syscall(nr::SYS_READAHEAD, fs::sys_readahead);
    register_syscall(nr::SYS_FADVISE64, fs::sys_fadvise64);
    register_syscall(nr::SYS_FLOCK, fs::sys_flock);
    register_syscall(nr::SYS_PPOLL, fs::sys_ppoll);
    register_syscall(nr::SYS_PSELECT6, fs::sys_pselect6);
    register_syscall(nr::SYS_CLOSE_RANGE, fs::sys_close_range);
    register_syscall(nr::SYS_EVENTFD2, fs::sys_eventfd2);
    register_syscall(nr::SYS_TIMERFD_CREATE, fs::sys_timerfd_create);
    register_syscall(nr::SYS_TIMERFD_SETTIME, fs::sys_timerfd_settime);
    register_syscall(nr::SYS_TIMERFD_GETTIME, fs::sys_timerfd_gettime);
    register_syscall(nr::SYS_SIGNALFD4, fs::sys_signalfd4);
    register_syscall(nr::SYS_EPOLL_CREATE1, fs::sys_epoll_create1);
    register_syscall(nr::SYS_EPOLL_CTL, fs::sys_epoll_ctl);
    register_syscall(nr::SYS_EPOLL_PWAIT, fs::sys_epoll_pwait);
    register_syscall(nr::SYS_SOCKET, fs::sys_socket);
    register_syscall(nr::SYS_SOCKETPAIR, fs::sys_socketpair);
    register_syscall(nr::SYS_BIND, fs::sys_bind);
    register_syscall(nr::SYS_LISTEN, fs::sys_listen);
    register_syscall(nr::SYS_ACCEPT, fs::sys_accept);
    register_syscall(nr::SYS_ACCEPT4, fs::sys_accept4);
    register_syscall(nr::SYS_RECVMMSG, fs::sys_recvmmsg);
    register_syscall(nr::SYS_CONNECT, fs::sys_connect);
    register_syscall(nr::SYS_GETSOCKNAME, fs::sys_getsockname);
    register_syscall(nr::SYS_GETPEERNAME, fs::sys_getpeername);
    register_syscall(nr::SYS_SENDTO, fs::sys_sendto);
    register_syscall(nr::SYS_RECVFROM, fs::sys_recvfrom);
    register_syscall(nr::SYS_SENDMSG, fs::sys_sendmsg);
    register_syscall(nr::SYS_RECVMSG, fs::sys_recvmsg);
    register_syscall(nr::SYS_SENDMMSG, fs::sys_sendmmsg);
    register_syscall(nr::SYS_SETSOCKOPT, fs::sys_setsockopt);
    register_syscall(nr::SYS_GETSOCKOPT, fs::sys_getsockopt);
    register_syscall(nr::SYS_SHUTDOWN, fs::sys_shutdown);
    register_syscall(nr::SYS_FACCESSAT, fs::sys_faccessat);
    register_syscall(nr::SYS_FACCESSAT2, fs::sys_faccessat2);
    register_syscall(nr::SYS_OPENAT, fs::sys_openat);
    register_syscall(nr::SYS_WRITE, fs::sys_write);
    register_syscall(nr::SYS_WRITEV, fs::sys_writev);
    register_syscall(nr::SYS_READ, fs::sys_read);
    register_syscall(nr::SYS_READV, fs::sys_readv);
    register_syscall(nr::SYS_PREAD64, fs::sys_pread64);
    register_syscall(nr::SYS_PWRITE64, fs::sys_pwrite64);
    register_syscall(nr::SYS_CLOSE, fs::sys_close);
    register_syscall(nr::SYS_LSEEK, fs::sys_lseek);
    register_syscall(nr::SYS_READLINKAT, fs::sys_readlinkat);
    register_syscall(nr::SYS_NEWFSTATAT, fs::sys_newfstatat);
    register_syscall(nr::SYS_FSTAT, fs::sys_fstat);
    register_syscall(nr::SYS_STATX, fs::sys_statx);

    // 进程
    register_syscall(nr::SYS_EXIT, process::sys_exit);
    register_syscall(nr::SYS_EXIT_GROUP, process::sys_exit_group);
    register_syscall(nr::SYS_CLONE, process::sys_clone);
    register_syscall(nr::SYS_CLONE3, process::sys_clone3);
    register_syscall(nr::SYS_EXECVE, process::sys_execve);
    register_syscall(nr::SYS_WAIT4, process::sys_wait4);
    register_syscall(nr::SYS_WAITID, process::sys_waitid);
    register_syscall(nr::SYS_SET_TID_ADDRESS, process::sys_set_tid_address);
    register_syscall(nr::SYS_SET_ROBUST_LIST, process::sys_set_robust_list);
    register_syscall(nr::SYS_SCHED_YIELD, process::sys_sched_yield);
    register_syscall(nr::SYS_KILL, process::sys_kill);
    register_syscall(nr::SYS_TKILL, process::sys_tkill);
    register_syscall(nr::SYS_TGKILL, process::sys_tgkill);
    register_syscall(nr::SYS_CLOCK_GETTIME, process::sys_clock_gettime);
    register_syscall(nr::SYS_SETPGID, process::sys_setpgid);
    register_syscall(nr::SYS_GETPGID, process::sys_getpgid);
    register_syscall(nr::SYS_GETSID, process::sys_getsid);
    register_syscall(nr::SYS_SETSID, process::sys_setsid);
    register_syscall(nr::SYS_UNAME, process::sys_uname);
    register_syscall(nr::SYS_GETCPU, process::sys_getcpu);
    register_syscall(nr::SYS_GETPID, process::sys_getpid);
    register_syscall(nr::SYS_GETTID, process::sys_gettid);
    register_syscall(nr::SYS_GETPPID, process::sys_getppid);
    register_syscall(nr::SYS_GETUID, process::sys_getuid);
    register_syscall(nr::SYS_GETEUID, process::sys_geteuid);
    register_syscall(nr::SYS_GETGID, process::sys_getgid);
    register_syscall(nr::SYS_GETEGID, process::sys_getegid);
    register_syscall(nr::SYS_PRLIMIT64, process::sys_prlimit64);
    register_syscall(nr::SYS_GETRLIMIT, process::sys_getrlimit);
    register_syscall(nr::SYS_SETRLIMIT, process::sys_setrlimit);
    register_syscall(nr::SYS_GETRANDOM, process::sys_getrandom);
    register_syscall(nr::SYS_NANOSLEEP, process::sys_nanosleep);
    register_syscall(nr::SYS_CLOCK_NANOSLEEP, process::sys_clock_nanosleep);
    register_syscall(nr::SYS_CLOCK_GETRES, process::sys_clock_getres);
    register_syscall(nr::SYS_TIMES, process::sys_times);
    register_syscall(nr::SYS_GETRUSAGE, process::sys_getrusage);
    register_syscall(nr::SYS_SYSINFO, process::sys_sysinfo);
    register_syscall(nr::SYS_SYSLOG, syslog::sys_syslog);
    register_syscall(nr::SYS_GETTIMEOFDAY, process::sys_gettimeofday);
    register_syscall(nr::SYS_GETPRIORITY, process::sys_getpriority);
    register_syscall(nr::SYS_SETPRIORITY, process::sys_setpriority);
    register_syscall(nr::SYS_REBOOT, process::sys_reboot);
    register_syscall(nr::SYS_SCHED_GETPARAM, process::sys_sched_getparam);
    register_syscall(nr::SYS_SCHED_SETPARAM, process::sys_sched_setparam);
    register_syscall(nr::SYS_SCHED_GETSCHEDULER, process::sys_sched_getscheduler);
    register_syscall(nr::SYS_SCHED_SETSCHEDULER, process::sys_sched_setscheduler);
    register_syscall(nr::SYS_SCHED_SETAFFINITY, process::sys_sched_setaffinity);
    register_syscall(nr::SYS_SCHED_GETAFFINITY, process::sys_sched_getaffinity);
    register_syscall(
        nr::SYS_SCHED_GET_PRIORITY_MAX,
        process::sys_sched_get_priority_max,
    );
    register_syscall(
        nr::SYS_SCHED_GET_PRIORITY_MIN,
        process::sys_sched_get_priority_min,
    );
    register_syscall(nr::SYS_PERSONALITY, process::sys_personality);
    register_syscall(nr::SYS_PRCTL, process::sys_prctl);
    register_syscall(nr::SYS_CAPGET, process::sys_capget);
    register_syscall(nr::SYS_CAPSET, process::sys_capset);
    register_syscall(nr::SYS_SETUID, process::sys_setuid);
    register_syscall(nr::SYS_SETGID, process::sys_setgid);
    register_syscall(nr::SYS_SETREUID, process::sys_setreuid);
    register_syscall(nr::SYS_SETREGID, process::sys_setregid);
    register_syscall(nr::SYS_SETRESUID, process::sys_setresuid);
    register_syscall(nr::SYS_SETRESGID, process::sys_setresgid);
    register_syscall(nr::SYS_SETFSUID, process::sys_setfsuid);
    register_syscall(nr::SYS_SETFSGID, process::sys_setfsgid);
    register_syscall(nr::SYS_GETGROUPS, process::sys_getgroups);
    register_syscall(nr::SYS_SETGROUPS, process::sys_setgroups);
    register_syscall(nr::SYS_FUTEX, process::sys_futex);

    // 内存
    register_syscall(nr::SYS_BRK, mm::sys_brk);
    register_syscall(nr::SYS_MMAP, mm::sys_mmap);
    register_syscall(nr::SYS_MUNMAP, mm::sys_munmap);
    register_syscall(nr::SYS_MPROTECT, mm::sys_mprotect);
    register_syscall(nr::SYS_MADVISE, mm::sys_madvise);
    register_syscall(nr::SYS_MREMAP, mm::sys_mremap);

    // SysV IPC
    register_syscall(nr::SYS_SHMGET, ipc::sys_shmget);
    register_syscall(nr::SYS_SHMCTL, ipc::sys_shmctl);
    register_syscall(nr::SYS_SHMAT, ipc::sys_shmat);
    register_syscall(nr::SYS_SHMDT, ipc::sys_shmdt);

    // 信号
    register_syscall(nr::SYS_RT_SIGACTION, signal::sys_rt_sigaction);
    register_syscall(nr::SYS_RT_SIGPROCMASK, signal::sys_rt_sigprocmask);
    register_syscall(nr::SYS_RT_SIGPENDING, signal::sys_rt_sigpending);
    register_syscall(nr::SYS_RT_SIGRETURN, signal::sys_rt_sigreturn);
    register_syscall(nr::SYS_RT_SIGSUSPEND, signal::sys_rt_sigsuspend);
    register_syscall(nr::SYS_RT_SIGTIMEDWAIT, signal::sys_rt_sigtimedwait);
    register_syscall(nr::SYS_SIGALTSTACK, signal::sys_sigaltstack);
    register_syscall(nr::SYS_RESTART_SYSCALL, signal::sys_restart_syscall);

    log::info!(
        "[syscalls] registered {} entries",
        general::syscall::registered_count()
    );
}

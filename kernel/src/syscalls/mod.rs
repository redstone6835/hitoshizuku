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
    register_syscall(nr::SYS_FCHMOD, fs::sys_fchmod);
    register_syscall(nr::SYS_FCHMODAT, fs::sys_fchmodat);
    register_syscall(nr::SYS_FCHOWNAT, fs::sys_fchownat);
    register_syscall(nr::SYS_FCHOWN, fs::sys_fchown);
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
    register_syscall(nr::SYS_FSTATAT, fs::sys_newfstatat);
    register_syscall(nr::SYS_FSTAT, fs::sys_fstat);
    register_syscall(nr::SYS_STATX, fs::sys_statx);
    register_syscall(nr::SYS_SETXATTR, fs::sys_setxattr);
    register_syscall(nr::SYS_LSETXATTR, fs::sys_lsetxattr);
    register_syscall(nr::SYS_FSETXATTR, fs::sys_fsetxattr);
    register_syscall(nr::SYS_GETXATTR, fs::sys_getxattr);
    register_syscall(nr::SYS_LGETXATTR, fs::sys_lgetxattr);
    register_syscall(nr::SYS_FGETXATTR, fs::sys_fgetxattr);
    register_syscall(nr::SYS_LISTXATTR, fs::sys_listxattr);
    register_syscall(nr::SYS_LLISTXATTR, fs::sys_llistxattr);
    register_syscall(nr::SYS_FLISTXATTR, fs::sys_flistxattr);
    register_syscall(nr::SYS_REMOVEXATTR, fs::sys_removexattr);
    register_syscall(nr::SYS_LREMOVEXATTR, fs::sys_lremovexattr);
    register_syscall(nr::SYS_FREMOVEXATTR, fs::sys_fremovexattr);
    register_syscall(nr::SYS_LOOKUP_DCOOKIE, fs::sys_lookup_dcookie);
    register_syscall(nr::SYS_INOTIFY_INIT1, fs::sys_inotify_init1);
    register_syscall(nr::SYS_INOTIFY_ADD_WATCH, fs::sys_inotify_add_watch);
    register_syscall(nr::SYS_INOTIFY_RM_WATCH, fs::sys_inotify_rm_watch);
    register_syscall(nr::SYS_IOPRIO_SET, fs::sys_ioprio_set);
    register_syscall(nr::SYS_IOPRIO_GET, fs::sys_ioprio_get);
    register_syscall(nr::SYS_RENAMEAT, fs::sys_renameat);
    register_syscall(nr::SYS_NFSSERVCTL, fs::sys_nfsservctl);
    register_syscall(nr::SYS_VHANGUP, fs::sys_vhangup);
    register_syscall(nr::SYS_QUOTACTL, fs::sys_quotactl);
    register_syscall(nr::SYS_PREADV, fs::sys_preadv);
    register_syscall(nr::SYS_PWRITEV, fs::sys_pwritev);
    register_syscall(nr::SYS_VMSPLICE, fs::sys_vmsplice);
    register_syscall(nr::SYS_SPLICE, fs::sys_splice);
    register_syscall(nr::SYS_TEE, fs::sys_tee);
    register_syscall(nr::SYS_SYNC_FILE_RANGE2, fs::sys_sync_file_range2);
    register_syscall(nr::SYS_ACCT, fs::sys_acct);
    register_syscall(nr::SYS_FANOTIFY_INIT, fs::sys_fanotify_init);
    register_syscall(nr::SYS_FANOTIFY_MARK, fs::sys_fanotify_mark);
    register_syscall(nr::SYS_NAME_TO_HANDLE_AT, fs::sys_name_to_handle_at);
    register_syscall(nr::SYS_OPEN_BY_HANDLE_AT, fs::sys_open_by_handle_at);
    register_syscall(nr::SYS_MEMFD_CREATE, fs::sys_memfd_create);
    register_syscall(nr::SYS_PREADV2, fs::sys_preadv2);
    register_syscall(nr::SYS_PWRITEV2, fs::sys_pwritev2);
    register_syscall(nr::SYS_TIMERFD_GETTIME64, fs::sys_timerfd_gettime64);
    register_syscall(nr::SYS_TIMERFD_SETTIME64, fs::sys_timerfd_settime64);
    register_syscall(nr::SYS_UTIMENSAT_TIME64, fs::sys_utimensat_time64);
    register_syscall(nr::SYS_PSELECT6_TIME64, fs::sys_pselect6_time64);
    register_syscall(nr::SYS_PPOLL_TIME64, fs::sys_ppoll_time64);
    register_syscall(nr::SYS_RECVMMSG_TIME64, fs::sys_recvmmsg_time64);
    register_syscall(nr::SYS_IO_URING_SETUP, fs::sys_io_uring_setup);
    register_syscall(nr::SYS_IO_URING_ENTER, fs::sys_io_uring_enter);
    register_syscall(nr::SYS_IO_URING_REGISTER, fs::sys_io_uring_register);
    register_syscall(nr::SYS_OPEN_TREE, fs::sys_open_tree);
    register_syscall(nr::SYS_MOVE_MOUNT, fs::sys_move_mount);
    register_syscall(nr::SYS_FSOPEN, fs::sys_fsopen);
    register_syscall(nr::SYS_FSCONFIG, fs::sys_fsconfig);
    register_syscall(nr::SYS_FSMOUNT, fs::sys_fsmount);
    register_syscall(nr::SYS_FSPICK, fs::sys_fspick);
    register_syscall(nr::SYS_OPENAT2, fs::sys_openat2);
    register_syscall(nr::SYS_PIDFD_GETFD, fs::sys_pidfd_getfd);
    register_syscall(nr::SYS_EPOLL_PWAIT2, fs::sys_epoll_pwait2);
    register_syscall(nr::SYS_MOUNT_SETATTR, fs::sys_mount_setattr);
    register_syscall(nr::SYS_QUOTACTL_FD, fs::sys_quotactl_fd);
    register_syscall(nr::SYS_FCHMODAT2, fs::sys_fchmodat2);
    register_syscall(nr::SYS_STATMOUNT, fs::sys_statmount);
    register_syscall(nr::SYS_LISTMOUNT, fs::sys_listmount);
    register_syscall(nr::SYS_SETXATTRAT, fs::sys_setxattrat);
    register_syscall(nr::SYS_GETXATTRAT, fs::sys_getxattrat);
    register_syscall(nr::SYS_LISTXATTRAT, fs::sys_listxattrat);
    register_syscall(nr::SYS_REMOVEXATTRAT, fs::sys_removexattrat);
    register_syscall(nr::SYS_OPEN_TREE_ATTR, fs::sys_open_tree_attr);
    register_syscall(nr::SYS_FILE_GETATTR, fs::sys_file_getattr);
    register_syscall(nr::SYS_FILE_SETATTR, fs::sys_file_setattr);

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
    register_syscall(nr::SYS_GET_ROBUST_LIST, process::sys_get_robust_list);
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
    register_syscall(nr::SYS_GETITIMER, process::sys_getitimer);
    register_syscall(nr::SYS_SETITIMER, process::sys_setitimer);
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
    register_syscall(
        nr::SYS_SCHED_RR_GET_INTERVAL,
        process::sys_sched_rr_get_interval,
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
    register_syscall(nr::SYS_RT_TGSIGQUEUEINFO, process::sys_rt_tgsigqueueinfo);
    register_syscall(nr::SYS_SCHED_SETATTR, process::sys_sched_setattr);
    register_syscall(nr::SYS_SCHED_GETATTR, process::sys_sched_getattr);
    register_syscall(nr::SYS_MYGO_SCHED_INFO, process::sys_mygo_sched_info);
    register_syscall(nr::SYS_MEMBARRIER, process::sys_membarrier);
    register_syscall(nr::SYS_FUTEX, process::sys_futex);
    register_syscall(nr::SYS_RSEQ, process::sys_rseq);
    register_syscall(nr::SYS_FUTEX_TIME64, process::sys_futex);
    register_syscall(
        nr::SYS_SCHED_RR_GET_INTERVAL_TIME64,
        process::sys_sched_rr_get_interval,
    );
    register_syscall(nr::SYS_FUTEX_WAITV, process::sys_futex_waitv);
    register_syscall(nr::SYS_FUTEX_WAKE, process::sys_futex_wake);
    register_syscall(nr::SYS_FUTEX_WAIT, process::sys_futex_wait);
    register_syscall(nr::SYS_FUTEX_REQUEUE, process::sys_futex_requeue);
    register_syscall(nr::SYS_UNSHARE, process::sys_unshare);
    register_syscall(nr::SYS_KEXEC_LOAD, process::sys_kexec_load);
    register_syscall(nr::SYS_INIT_MODULE, process::sys_init_module);
    register_syscall(nr::SYS_DELETE_MODULE, process::sys_delete_module);
    register_syscall(nr::SYS_TIMER_CREATE, process::sys_timer_create);
    register_syscall(nr::SYS_TIMER_GETTIME, process::sys_timer_gettime);
    register_syscall(nr::SYS_TIMER_GETOVERRUN, process::sys_timer_getoverrun);
    register_syscall(nr::SYS_TIMER_SETTIME, process::sys_timer_settime);
    register_syscall(nr::SYS_TIMER_DELETE, process::sys_timer_delete);
    register_syscall(nr::SYS_CLOCK_SETTIME, process::sys_clock_settime);
    register_syscall(nr::SYS_PTRACE, process::sys_ptrace);
    register_syscall(nr::SYS_GETRESUID, process::sys_getresuid);
    register_syscall(nr::SYS_GETRESGID, process::sys_getresgid);
    register_syscall(nr::SYS_SETHOSTNAME, process::sys_sethostname);
    register_syscall(nr::SYS_SETDOMAINNAME, process::sys_setdomainname);
    register_syscall(nr::SYS_UMASK, process::sys_umask);
    register_syscall(nr::SYS_SETTIMEOFDAY, process::sys_settimeofday);
    register_syscall(nr::SYS_ADJTIMEX, process::sys_adjtimex);
    register_syscall(nr::SYS_PERF_EVENT_OPEN, process::sys_perf_event_open);
    register_syscall(nr::SYS_CLOCK_ADJTIME, process::sys_clock_adjtime);
    register_syscall(nr::SYS_SETNS, process::sys_setns);
    register_syscall(nr::SYS_KCMP, process::sys_kcmp);
    register_syscall(nr::SYS_FINIT_MODULE, process::sys_finit_module);
    register_syscall(nr::SYS_SECCOMP, process::sys_seccomp);
    register_syscall(nr::SYS_BPF, process::sys_bpf);
    register_syscall(nr::SYS_EXECVEAT, process::sys_execveat);
    register_syscall(nr::SYS_KEXEC_FILE_LOAD, process::sys_kexec_file_load);
    register_syscall(nr::SYS_CLOCK_GETTIME64, process::sys_clock_gettime64);
    register_syscall(nr::SYS_CLOCK_SETTIME64, process::sys_clock_settime64);
    register_syscall(nr::SYS_CLOCK_ADJTIME64, process::sys_clock_adjtime64);
    register_syscall(
        nr::SYS_CLOCK_GETRES_TIME64,
        process::sys_clock_getres_time64,
    );
    register_syscall(
        nr::SYS_CLOCK_NANOSLEEP_TIME64,
        process::sys_clock_nanosleep_time64,
    );
    register_syscall(nr::SYS_TIMER_GETTIME64, process::sys_timer_gettime64);
    register_syscall(nr::SYS_TIMER_SETTIME64, process::sys_timer_settime64);
    register_syscall(nr::SYS_PIDFD_OPEN, process::sys_pidfd_open);
    register_syscall(
        nr::SYS_LANDLOCK_CREATE_RULESET,
        process::sys_landlock_create_ruleset,
    );
    register_syscall(nr::SYS_LANDLOCK_ADD_RULE, process::sys_landlock_add_rule);
    register_syscall(
        nr::SYS_LANDLOCK_RESTRICT_SELF,
        process::sys_landlock_restrict_self,
    );
    register_syscall(nr::SYS_PROCESS_MRELEASE, process::sys_process_mrelease);
    register_syscall(nr::SYS_LSM_GET_SELF_ATTR, process::sys_lsm_get_self_attr);
    register_syscall(nr::SYS_LSM_SET_SELF_ATTR, process::sys_lsm_set_self_attr);
    register_syscall(nr::SYS_LSM_LIST_MODULES, process::sys_lsm_list_modules);

    // 内存
    register_syscall(nr::SYS_BRK, mm::sys_brk);
    register_syscall(nr::SYS_MMAP, mm::sys_mmap);
    register_syscall(nr::SYS_MUNMAP, mm::sys_munmap);
    register_syscall(nr::SYS_MPROTECT, mm::sys_mprotect);
    register_syscall(nr::SYS_MADVISE, mm::sys_madvise);
    register_syscall(nr::SYS_MREMAP, mm::sys_mremap);
    register_syscall(nr::SYS_SWAPON, mm::sys_swapon);
    register_syscall(nr::SYS_SWAPOFF, mm::sys_swapoff);
    register_syscall(nr::SYS_MSYNC, mm::sys_msync);
    register_syscall(nr::SYS_MLOCK, mm::sys_mlock);
    register_syscall(nr::SYS_MUNLOCK, mm::sys_munlock);
    register_syscall(nr::SYS_MLOCKALL, mm::sys_mlockall);
    register_syscall(nr::SYS_MUNLOCKALL, mm::sys_munlockall);
    register_syscall(nr::SYS_MINCORE, mm::sys_mincore);
    register_syscall(nr::SYS_REMAP_FILE_PAGES, mm::sys_remap_file_pages);
    register_syscall(nr::SYS_MBIND, mm::sys_mbind);
    register_syscall(nr::SYS_GET_MEMPOLICY, mm::sys_get_mempolicy);
    register_syscall(nr::SYS_SET_MEMPOLICY, mm::sys_set_mempolicy);
    register_syscall(nr::SYS_MIGRATE_PAGES, mm::sys_migrate_pages);
    register_syscall(nr::SYS_MOVE_PAGES, mm::sys_move_pages);
    register_syscall(nr::SYS_PROCESS_VM_READV, mm::sys_process_vm_readv);
    register_syscall(nr::SYS_PROCESS_VM_WRITEV, mm::sys_process_vm_writev);
    register_syscall(nr::SYS_USERFAULTFD, mm::sys_userfaultfd);
    register_syscall(nr::SYS_MLOCK2, mm::sys_mlock2);
    register_syscall(nr::SYS_PKEY_MPROTECT, mm::sys_pkey_mprotect);
    register_syscall(nr::SYS_PKEY_ALLOC, mm::sys_pkey_alloc);
    register_syscall(nr::SYS_PKEY_FREE, mm::sys_pkey_free);
    register_syscall(nr::SYS_PROCESS_MADVISE, mm::sys_process_madvise);
    register_syscall(nr::SYS_MEMFD_SECRET, mm::sys_memfd_secret);
    register_syscall(
        nr::SYS_SET_MEMPOLICY_HOME_NODE,
        mm::sys_set_mempolicy_home_node,
    );
    register_syscall(nr::SYS_CACHESTAT, mm::sys_cachestat);
    register_syscall(nr::SYS_MAP_SHADOW_STACK, mm::sys_map_shadow_stack);
    register_syscall(nr::SYS_MSEAL, mm::sys_mseal);

    // SysV IPC
    register_syscall(nr::SYS_SHMGET, ipc::sys_shmget);
    register_syscall(nr::SYS_SHMCTL, ipc::sys_shmctl);
    register_syscall(nr::SYS_SHMAT, ipc::sys_shmat);
    register_syscall(nr::SYS_SHMDT, ipc::sys_shmdt);
    register_syscall(nr::SYS_IO_SETUP, ipc::sys_io_setup);
    register_syscall(nr::SYS_IO_DESTROY, ipc::sys_io_destroy);
    register_syscall(nr::SYS_IO_SUBMIT, ipc::sys_io_submit);
    register_syscall(nr::SYS_IO_CANCEL, ipc::sys_io_cancel);
    register_syscall(nr::SYS_IO_GETEVENTS, ipc::sys_io_getevents);
    register_syscall(nr::SYS_MQ_OPEN, ipc::sys_mq_open);
    register_syscall(nr::SYS_MQ_UNLINK, ipc::sys_mq_unlink);
    register_syscall(nr::SYS_MQ_TIMEDSEND, ipc::sys_mq_timedsend);
    register_syscall(nr::SYS_MQ_TIMEDRECEIVE, ipc::sys_mq_timedreceive);
    register_syscall(nr::SYS_MQ_NOTIFY, ipc::sys_mq_notify);
    register_syscall(nr::SYS_MQ_GETSETATTR, ipc::sys_mq_getsetattr);
    register_syscall(nr::SYS_MSGGET, ipc::sys_msgget);
    register_syscall(nr::SYS_MSGCTL, ipc::sys_msgctl);
    register_syscall(nr::SYS_MSGRCV, ipc::sys_msgrcv);
    register_syscall(nr::SYS_MSGSND, ipc::sys_msgsnd);
    register_syscall(nr::SYS_SEMGET, ipc::sys_semget);
    register_syscall(nr::SYS_SEMCTL, ipc::sys_semctl);
    register_syscall(nr::SYS_SEMTIMEDOP, ipc::sys_semtimedop);
    register_syscall(nr::SYS_SEMOP, ipc::sys_semop);
    register_syscall(nr::SYS_ADD_KEY, ipc::sys_add_key);
    register_syscall(nr::SYS_REQUEST_KEY, ipc::sys_request_key);
    register_syscall(nr::SYS_KEYCTL, ipc::sys_keyctl);
    register_syscall(nr::SYS_IO_PGETEVENTS, ipc::sys_io_pgetevents);
    register_syscall(nr::SYS_IO_PGETEVENTS_TIME64, ipc::sys_io_pgetevents_time64);
    register_syscall(nr::SYS_MQ_TIMEDSEND_TIME64, ipc::sys_mq_timedsend_time64);
    register_syscall(
        nr::SYS_MQ_TIMEDRECEIVE_TIME64,
        ipc::sys_mq_timedreceive_time64,
    );
    register_syscall(nr::SYS_SEMTIMEDOP_TIME64, ipc::sys_semtimedop_time64);

    // 信号
    register_syscall(nr::SYS_RT_SIGACTION, signal::sys_rt_sigaction);
    register_syscall(nr::SYS_RT_SIGPROCMASK, signal::sys_rt_sigprocmask);
    register_syscall(nr::SYS_RT_SIGPENDING, signal::sys_rt_sigpending);
    register_syscall(nr::SYS_RT_SIGRETURN, signal::sys_rt_sigreturn);
    register_syscall(nr::SYS_RT_SIGSUSPEND, signal::sys_rt_sigsuspend);
    register_syscall(nr::SYS_RT_SIGTIMEDWAIT, signal::sys_rt_sigtimedwait);
    register_syscall(nr::SYS_SIGALTSTACK, signal::sys_sigaltstack);
    register_syscall(nr::SYS_RESTART_SYSCALL, signal::sys_restart_syscall);
    register_syscall(nr::SYS_RT_SIGQUEUEINFO, signal::sys_rt_sigqueueinfo);
    register_syscall(
        nr::SYS_RT_SIGTIMEDWAIT_TIME64,
        signal::sys_rt_sigtimedwait_time64,
    );
    register_syscall(nr::SYS_PIDFD_SEND_SIGNAL, signal::sys_pidfd_send_signal);

    let registered = general::syscall::registered_count();
    log::info!(
        "[syscalls] registered {} entries",
        registered
    );
}

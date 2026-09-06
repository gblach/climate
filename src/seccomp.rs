use anyhow::{Context, Result, bail};
use libseccomp::ScmpSyscall;
use oci_spec::runtime::{
    Arch, LinuxSeccomp, LinuxSeccompAction, LinuxSeccompArg, LinuxSeccompArgBuilder,
    LinuxSeccompBuilder, LinuxSeccompOperator, LinuxSyscall, LinuxSyscallBuilder,
};
use std::collections::BTreeSet;

use crate::config::{Capability, RunConfig};

// The syscalls an app is allowed to make. This is the list the default profile of docker and
// podman allows, as shipped in /usr/share/containers/seccomp.json, minus the few that the rules
// further down handle on their own. Anything not named here fails with EPERM, which includes every
// syscall a kernel newer than this list adds. The formatter is kept off it: one name per line
// would run to several hundred and bury the rules underneath.
#[rustfmt::skip]
const ALLOWED: [&str; 366] = [
    "_llseek", "_newselect", "accept", "accept4", "access", "adjtimex", "alarm", "bind", "brk",
    "capget", "capset", "chdir", "chmod", "chown", "chown32", "clock_adjtime", "clock_adjtime64",
    "clock_getres", "clock_getres_time64", "clock_gettime", "clock_gettime64", "clock_nanosleep",
    "clock_nanosleep_time64", "close", "close_range", "connect", "copy_file_range", "creat", "dup",
    "dup2", "dup3", "epoll_create", "epoll_create1", "epoll_ctl", "epoll_ctl_old", "epoll_pwait",
    "epoll_pwait2", "epoll_wait", "epoll_wait_old", "eventfd", "eventfd2", "execve", "execveat",
    "exit", "exit_group", "faccessat", "faccessat2", "fadvise64", "fadvise64_64", "fallocate",
    "fanotify_init", "fanotify_mark", "fchdir", "fchmod", "fchmodat", "fchmodat2", "fchown",
    "fchown32", "fchownat", "fcntl", "fcntl64", "fdatasync", "fgetxattr", "flistxattr", "flock",
    "fork", "fremovexattr", "fsconfig", "fsetxattr", "fsmount", "fsopen", "fspick", "fstat",
    "fstat64", "fstatat64", "fstatfs", "fstatfs64", "fsync", "ftruncate", "ftruncate64", "futex",
    "futex_time64", "futimesat", "get_mempolicy", "get_robust_list", "get_thread_area", "getcpu",
    "getcwd", "getdents", "getdents64", "getegid", "getegid32", "geteuid", "geteuid32", "getgid",
    "getgid32", "getgroups", "getgroups32", "getitimer", "getpeername", "getpgid", "getpgrp",
    "getpid", "getppid", "getpriority", "getrandom", "getresgid", "getresgid32", "getresuid",
    "getresuid32", "getrlimit", "getrusage", "getsid", "getsockname", "getsockopt", "gettid",
    "gettimeofday", "getuid", "getuid32", "getxattr", "inotify_add_watch", "inotify_init",
    "inotify_init1", "inotify_rm_watch", "io_cancel", "io_destroy", "io_getevents", "io_setup",
    "io_submit", "ioctl", "ioprio_get", "ioprio_set", "ipc", "keyctl", "kill", "landlock_add_rule",
    "landlock_create_ruleset", "landlock_restrict_self", "lchown", "lchown32", "lgetxattr", "link",
    "linkat", "listen", "listxattr", "llistxattr", "lremovexattr", "lseek", "lsetxattr", "lstat",
    "lstat64", "madvise", "mbind", "membarrier", "memfd_create", "memfd_secret", "mincore", "mkdir",
    "mkdirat", "mknod", "mknodat", "mlock", "mlock2", "mlockall", "mmap", "mmap2", "mount",
    "mount_setattr", "move_mount", "mprotect", "mq_getsetattr", "mq_notify", "mq_open",
    "mq_timedreceive", "mq_timedreceive_time64", "mq_timedsend", "mq_timedsend_time64", "mq_unlink",
    "mremap", "msgctl", "msgget", "msgrcv", "msgsnd", "msync", "munlock", "munlockall", "munmap",
    "name_to_handle_at", "nanosleep", "newfstatat", "open", "open_tree", "openat", "openat2",
    "pause", "pidfd_getfd", "pidfd_open", "pidfd_send_signal", "pipe", "pipe2", "pivot_root",
    "pkey_alloc", "pkey_free", "pkey_mprotect", "poll", "ppoll", "ppoll_time64", "prctl", "pread64",
    "preadv", "preadv2", "prlimit64", "process_mrelease", "process_vm_readv", "process_vm_writev",
    "pselect6", "pselect6_time64", "ptrace", "pwrite64", "pwritev", "pwritev2", "read", "readahead",
    "readlink", "readlinkat", "readv", "reboot", "recv", "recvfrom", "recvmmsg", "recvmmsg_time64",
    "recvmsg", "remap_file_pages", "removexattr", "rename", "renameat", "renameat2",
    "restart_syscall", "rmdir", "rseq", "rt_sigaction", "rt_sigpending", "rt_sigprocmask",
    "rt_sigqueueinfo", "rt_sigreturn", "rt_sigsuspend", "rt_sigtimedwait", "rt_sigtimedwait_time64",
    "rt_tgsigqueueinfo", "sched_get_priority_max", "sched_get_priority_min", "sched_getaffinity",
    "sched_getattr", "sched_getparam", "sched_getscheduler", "sched_rr_get_interval",
    "sched_rr_get_interval_time64", "sched_setaffinity", "sched_setattr", "sched_setparam",
    "sched_setscheduler", "sched_yield", "seccomp", "select", "semctl", "semget", "semop",
    "semtimedop", "semtimedop_time64", "send", "sendfile", "sendfile64", "sendmmsg", "sendmsg",
    "sendto", "set_mempolicy", "set_robust_list", "set_thread_area", "set_tid_address", "setfsgid",
    "setfsgid32", "setfsuid", "setfsuid32", "setgid", "setgid32", "setgroups", "setgroups32",
    "setitimer", "setpgid", "setpriority", "setregid", "setregid32", "setresgid",
    "setresgid32", "setresuid", "setresuid32", "setreuid", "setreuid32", "setrlimit", "setsid",
    "setsockopt", "setuid", "setuid32", "setxattr", "shmat", "shmctl", "shmdt", "shmget",
    "shutdown", "sigaltstack", "signal", "signalfd", "signalfd4", "sigprocmask", "sigreturn",
    "socketcall", "socketpair", "splice", "stat", "stat64", "statfs", "statfs64", "statx",
    "symlink", "symlinkat", "sync", "sync_file_range", "syncfs", "sysinfo", "syslog", "tee",
    "tgkill", "time", "timer_create", "timer_delete", "timer_getoverrun", "timer_gettime",
    "timer_gettime64", "timer_settime", "timer_settime64", "timerfd_create", "timerfd_gettime",
    "timerfd_gettime64", "timerfd_settime", "timerfd_settime64", "times", "tkill", "truncate",
    "truncate64", "ugetrlimit", "umask", "umount", "umount2", "uname", "unlink", "unlinkat",
    "utime", "utimensat", "utimensat_time64", "utimes", "vfork", "wait4", "waitid", "waitpid",
    "write", "writev"
];

// Syscalls only some architectures have. The C library uses them, so leaving them out would stop
// every app from starting on the architectures that have them.
#[cfg(target_arch = "x86_64")]
const ARCH_ALLOWED: [&str; 2] = ["arch_prctl", "modify_ldt"];
#[cfg(target_arch = "aarch64")]
const ARCH_ALLOWED: [&str; 6] = [
    "arm_fadvise64_64",
    "arm_sync_file_range",
    "breakpoint",
    "cacheflush",
    "set_tls",
    "sync_file_range2",
];
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
const ARCH_ALLOWED: [&str; 0] = [];

// Which instruction sets the filter covers. A syscall made from an architecture the filter does
// not name is refused, so the 32-bit ones are listed too, or no 32-bit program could run at all.
#[cfg(target_arch = "x86_64")]
const ARCHITECTURES: [Arch; 3] = [Arch::ScmpArchX86_64, Arch::ScmpArchX86, Arch::ScmpArchX32];
#[cfg(target_arch = "aarch64")]
const ARCHITECTURES: [Arch; 2] = [Arch::ScmpArchAarch64, Arch::ScmpArchArm];
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
const ARCHITECTURES: [Arch; 1] = [Arch::ScmpArchNative];

// Syscalls the profile allows only when the app asked for the capability they belong to, paired
// the way the docker and podman profile pairs them. None appear in the list above, so this pairing
// is the only way to reach them; the kernel would refuse them without the capability anyway.
const CAPABILITY_SYSCALLS: [(Capability, &[&str]); 11] = [
    (Capability::Bpf, &["bpf"]),
    (Capability::DacReadSearch, &["open_by_handle_at"]),
    (Capability::Perfmon, &["perf_event_open"]),
    (
        Capability::SysAdmin,
        &[
            "bpf",
            "lookup_dcookie",
            "quotactl",
            "quotactl_fd",
            "setdomainname",
            "sethostname",
            "setns",
        ],
    ),
    (Capability::SysChroot, &["chroot"]),
    (
        Capability::SysModule,
        &[
            "delete_module",
            "finit_module",
            "init_module",
            "query_module",
        ],
    ),
    (Capability::SysPacct, &["acct"]),
    (Capability::SysPtrace, &["kcmp", "process_madvise"]),
    (Capability::SysRawio, &["ioperm", "iopl"]),
    (
        Capability::SysTime,
        &["clock_settime", "clock_settime64", "settimeofday", "stime"],
    ),
    (Capability::SysTtyConfig, &["vhangup"]),
];

// The flag that asks for a new user namespace. A process gets a full set of capabilities inside
// a user namespace it creates, which is how a container holding none could hand itself some back.
const CLONE_NEWUSER: u64 = 0x1000_0000;

// The address family of sockets that talk to the host of a virtual machine. Nothing a CLI tool
// needs, and it reaches past the container.
const AF_VSOCK: u64 = 40;

// "This kernel has no such syscall". clone3 keeps its flags in a structure in memory, which a
// seccomp filter cannot read, so the flag test below cannot be written for it. Answering with this
// error instead of EPERM makes the C library fall back to plain clone, which can be tested.
const ENOSYS: u32 = 38;

// The execution domains an app may switch to: the normal one, 32-bit, the "report an old kernel
// version" mode on its own and together with 32-bit, and the value that only reads the current
// domain back. What this leaves out is the domain that turns off address space randomization.
const PERSONALITIES: [u64; 5] = [0x0, 0x8, 0x20000, 0x20008, 0xffffffff];

// One test on one argument of a syscall: argument number `index` compared against `value` with
// `op`. A masked comparison is the odd one out - there the mask goes in `value_two`, and youki
// checks that (argument & value_two) equals `value`. The other operators ignore `value_two`.
fn condition(
    index: usize,
    op: LinuxSeccompOperator,
    value: u64,
    value_two: u64,
) -> Result<LinuxSeccompArg> {
    LinuxSeccompArgBuilder::default()
        .index(index)
        .value(value)
        .value_two(value_two)
        .op(op)
        .build()
        .context("building a seccomp argument test")
}

// A rule allowing the named syscalls with no strings attached.
fn allow(names: Vec<String>) -> Result<LinuxSyscall> {
    LinuxSyscallBuilder::default()
        .names(names)
        .action(LinuxSeccompAction::ScmpActAllow)
        .build()
        .context("building the allowed syscalls")
}

// A rule allowing one syscall only when every test holds. Calls that fail a test are left to the
// profile's default, which refuses them.
fn allow_if(name: &str, args: Vec<LinuxSeccompArg>) -> Result<LinuxSyscall> {
    LinuxSyscallBuilder::default()
        .names(vec![name.to_string()])
        .action(LinuxSeccompAction::ScmpActAllow)
        .args(args)
        .build()
        .with_context(|| format!("building the seccomp rule for {name}"))
}

// A rule answering one syscall with an error of its own rather than the profile's EPERM.
fn fail_with(name: &str, errno: u32) -> Result<LinuxSyscall> {
    LinuxSyscallBuilder::default()
        .names(vec![name.to_string()])
        .action(LinuxSeccompAction::ScmpActErrno)
        .errno_ret(errno)
        .build()
        .with_context(|| format!("building the seccomp rule for {name}"))
}

// The rules for syscalls that are allowed only in part. Each one is paired with the syscall it
// covers, so that an app naming that syscall in seccomp-allow or seccomp-deny can drop it.
fn partial_rules() -> Result<Vec<(&'static str, LinuxSyscall)>> {
    let mut rules = Vec::new();

    // Both of these create namespaces, and a user namespace is the one that must not be created.
    // The flags are the first argument of either call.
    for name in ["clone", "unshare"] {
        let no_user_namespace =
            condition(0, LinuxSeccompOperator::ScmpCmpMaskedEq, 0, CLONE_NEWUSER)?;
        rules.push((name, allow_if(name, vec![no_user_namespace])?));
    }
    rules.push(("clone3", fail_with("clone3", ENOSYS)?));

    let not_vsock = condition(0, LinuxSeccompOperator::ScmpCmpNe, AF_VSOCK, 0)?;
    rules.push(("socket", allow_if("socket", vec![not_vsock])?));

    // One rule per domain: the tests inside one rule all have to hold at once, while separate
    // rules for the same syscall are tried in turn, so a call matching any one of them is allowed.
    for personality in PERSONALITIES {
        let domain = condition(0, LinuxSeccompOperator::ScmpCmpEq, personality, 0)?;
        rules.push(("personality", allow_if("personality", vec![domain])?));
    }

    Ok(rules)
}

// The seccomp filter one app runs under: everything the profile does not name fails with EPERM.
// An app widens or narrows the list with seccomp-allow and seccomp-deny, and either key also drops
// the partial rule for a syscall it names, so that the app's own decision is the only one left.
pub fn profile(run: &RunConfig) -> Result<LinuxSeccomp> {
    let mut allowed: BTreeSet<&str> = ALLOWED.into_iter().chain(ARCH_ALLOWED).collect();
    for (capability, syscalls) in CAPABILITY_SYSCALLS {
        if run.capabilities.contains(&capability) {
            allowed.extend(syscalls);
        }
    }

    let mut partial = partial_rules()?;
    for name in &run.seccomp_allow {
        allowed.insert(name);
        partial.retain(|(covered, _)| covered != name);
    }
    for name in &run.seccomp_deny {
        allowed.remove(name.as_str());
        partial.retain(|(covered, _)| covered != name);
    }

    let mut syscalls = vec![allow(allowed.into_iter().map(String::from).collect())?];
    syscalls.extend(partial.into_iter().map(|(_, rule)| rule));

    LinuxSeccompBuilder::default()
        .default_action(LinuxSeccompAction::ScmpActErrno)
        .default_errno_ret(libc::EPERM as u32)
        .architectures(ARCHITECTURES.to_vec())
        .syscalls(syscalls)
        .build()
        .context("building the seccomp profile")
}

// Check the syscall names an app lists. A name the C library cannot resolve is dropped when the
// filter is built, so a misspelling in seccomp-deny would quietly permit what it meant to refuse.
pub fn check_syscall_names(run: &RunConfig) -> Result<()> {
    for name in run.seccomp_allow.iter().chain(&run.seccomp_deny) {
        if ScmpSyscall::from_name(name).is_err() {
            bail!("'{name}' is not the name of a syscall");
        }
    }
    Ok(())
}

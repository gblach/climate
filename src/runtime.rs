use anyhow::{Context, Result, anyhow, bail};
use libcontainer::container::builder::ContainerBuilder;
use libcontainer::syscall::syscall::SyscallType;
use oci_spec::runtime::Spec;
use rustix::io::Errno;
use rustix::net::{RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, recvmsg};
use rustix::pipe::{PipeFlags, pipe_with};
use rustix::process::{Pid, WaitOptions, getpid, set_child_subreaper};
use rustix::termios::{
    OptionalActions, Termios, Winsize, tcgetattr, tcgetwinsize, tcsetattr, tcsetwinsize,
};
use std::fs::File;
use std::io::{IoSliceMut, Read, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::thread::JoinHandle;

use crate::store;

// Scratch directory for running containers, under $XDG_RUNTIME_DIR or the temp directory. Each
// run creates overlays/, bundles/ and containers/ here and the `clean` command removes them,
// so both sides must agree on the location.
pub fn runtime_dir() -> PathBuf {
    dirs::runtime_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("climate")
}

// The size of the user's terminal, so the container's can match it; 0s if unknown.
fn window_size() -> Winsize {
    tcgetwinsize(std::io::stdin()).unwrap_or(Winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    })
}

// Copy bytes from one stream to the other until either end closes. A read error - which is what
// a closed terminal looks like - counts as a normal end.
fn copy(mut from: impl Read, mut to: impl Write) {
    let mut buf = [0u8; 8192];
    loop {
        match from.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if to.write_all(&buf[..n]).is_err() || to.flush().is_err() {
                    break;
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

// Switches the terminal to raw mode, where keystrokes are passed straight through instead of being
// line-buffered, and restores it when dropped.
struct RawMode {
    original: Termios,
}

impl RawMode {
    fn enable() -> Result<Self> {
        let stdin = std::io::stdin();
        let original = tcgetattr(stdin.as_fd()).context("reading terminal settings")?;
        let mut raw = original.clone();
        raw.make_raw();
        tcsetattr(stdin.as_fd(), OptionalActions::Now, &raw)
            .context("enabling raw terminal mode")?;
        Ok(Self { original })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = tcsetattr(
            std::io::stdin().as_fd(),
            OptionalActions::Now,
            &self.original,
        );
    }
}

// A local socket used to receive a terminal from the container runtime.
//
// A pty is a pair of connected endpoints that behaves like a terminal: youki creates one inside
// the container, hands the app one end as its terminal, and passes us the other. That is what makes
// Ctrl-C and Ctrl-Z turn into signals for the app; handing it our own stdin and stdout would not.
//
// An open file cannot be returned through a normal API, so it comes over this socket: we listen,
// youki connects while creating the container, and we then copy bytes between the endpoint it sent
// and the real terminal.
struct ConsoleSocket {
    listener: UnixListener,
    path: PathBuf,
}

impl ConsoleSocket {
    fn bind(dir: &Path) -> Result<Self> {
        let path = dir.join("console.sock");
        let listener = UnixListener::bind(&path)
            .with_context(|| format!("binding console socket {}", path.display()))?;
        Ok(Self { listener, path })
    }

    fn path(&self) -> &Path {
        &self.path
    }

    // Take our end of the container's terminal off the socket. youki sends it while the container
    // is created, but it waits in the socket's buffer, so accepting the connection afterwards still
    // finds it.
    fn into_master(self) -> Result<OwnedFd> {
        let (stream, _) = self
            .listener
            .accept()
            .context("accepting the console socket connection")?;
        let mut payload = [0u8; 256];
        let mut iov = [IoSliceMut::new(&mut payload)];
        let mut space =
            [const { std::mem::MaybeUninit::<u8>::uninit() }; rustix::cmsg_space!(ScmRights(1))];
        let mut ancillary = RecvAncillaryBuffer::new(&mut space);
        recvmsg(&stream, &mut iov, &mut ancillary, RecvFlags::empty())
            .context("receiving the pty master over the console socket")?;
        let master = ancillary
            .drain()
            .find_map(|msg| match msg {
                RecvAncillaryMessage::ScmRights(mut fds) => fds.next(),
                _ => None,
            })
            .context("console socket carried no pty master fd")?;
        let _ = tcsetwinsize(&master, window_size());
        Ok(master)
    }
}

// Start the two threads that shuttle bytes between the container's terminal and ours. The input
// thread runs until the process exits; the returned handle is the output thread's, which ends with
// the container and is waited for so that no output is lost.
fn pump(master: &OwnedFd) -> JoinHandle<()> {
    let writer = master
        .try_clone()
        .expect("duplicating the pty master for input");
    std::thread::spawn(move || copy(std::io::stdin(), File::from(writer)));

    let reader = master
        .try_clone()
        .expect("duplicating the pty master for output");
    std::thread::spawn(move || copy(File::from(reader), std::io::stdout()))
}

// Wait for the container's first process to finish and return its exit code, or 128 plus the signal
// number if it was killed. Other children may finish first; they are collected and ignored. Running
// out of children means that exit code is lost, which is an error, so a failed app can never look
// like it succeeded.
fn wait(pid: Pid) -> Result<i32> {
    loop {
        match rustix::process::wait(WaitOptions::empty()) {
            Ok(Some((p, status))) if p == pid => {
                if let Some(code) = status.exit_status() {
                    return Ok(code);
                }
                if let Some(signal) = status.terminating_signal() {
                    return Ok(128 + signal);
                }
            }
            Ok(_) => continue,
            Err(Errno::INTR) => continue,
            Err(Errno::CHILD) => bail!("the container exited but its exit status was lost"),
            Err(err) => return Err(err).context("waiting for the container"),
        }
    }
}

// Pid of the container's first process, read by the signal handler below (0 while there is none
// yet).
static CONTAINER_PID: AtomicI32 = AtomicI32::new(0);

// A signal that arrived before that process existed, delivered once it does.
static PENDING_SIGNAL: AtomicI32 = AtomicI32::new(0);

// Whether a signal has already been passed on to the container.
static FORWARDED: AtomicBool = AtomicBool::new(false);

// Pass Ctrl-C and `kill` (SIGINT/SIGTERM) on to the container instead of dying on the spot, so that
// it shuts down, `wait` returns, and the usual cleanup - deleting the container, unmounting
// the image, removing the bundle - still happens. Without this a signal during a non-interactive
// run would leave both the mount and the container behind. (Interactive runs never get here:
// in raw mode Ctrl-C is just a byte sent to the container's terminal.)
//
// The container's first process is PID 1 inside the container, and the kernel silently drops
// signals that PID 1 has no handler for, so a second signal is upgraded to SIGKILL, which cannot
// be ignored.
//
// Signal handlers may only call a small set of functions; these belong to it.
extern "C" fn forward_signal(signum: libc::c_int) {
    let pid = CONTAINER_PID.load(Ordering::Relaxed);
    if pid > 0 {
        let signum = match FORWARDED.swap(true, Ordering::Relaxed) {
            true => libc::SIGKILL,
            false => signum,
        };
        unsafe { libc::kill(pid, signum) };
    } else {
        PENDING_SIGNAL.store(signum, Ordering::Relaxed);
    }
}

fn install_signal_forwarding() -> Result<()> {
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = forward_signal as *const () as usize;
    for signum in [libc::SIGINT, libc::SIGTERM] {
        if unsafe { libc::sigaction(signum, &action, std::ptr::null_mut()) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("installing the handler for signal {signum}"));
        }
    }
    Ok(())
}

// Write end of a pipe to ourselves (-1 while there is none). A signal handler may not resize
// a terminal, so it only writes one byte here and the thread reading the other end does the work.
static RESIZE_PIPE: AtomicI32 = AtomicI32::new(-1);

extern "C" fn notify_resize(_signum: libc::c_int) {
    let fd = RESIZE_PIPE.load(Ordering::Relaxed);
    if fd >= 0 {
        unsafe { libc::write(fd, [0u8].as_ptr().cast(), 1) };
    }
}

// Keeps the container's terminal the same size as the user's for as long as an interactive
// run lasts. The kernel reports a resize with the SIGWINCH signal; copying the new size across
// makes it send that same signal on to the app, which is how full-screen programs know to redraw.
// Dropping this ends it.
struct ResizeForwarder {
    pipe: Option<OwnedFd>,
    thread: Option<JoinHandle<()>>,
}

impl ResizeForwarder {
    fn install(master: &OwnedFd) -> Result<Self> {
        let (read_end, write_end) =
            pipe_with(PipeFlags::CLOEXEC).context("creating the resize pipe")?;
        // Fail instead of blocking, so a full pipe cannot stall the handler.
        unsafe { libc::fcntl(write_end.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) };
        let master = master
            .try_clone()
            .context("duplicating the pty master for resizes")?;
        let mut reader = File::from(read_end);
        let thread = std::thread::spawn(move || {
            let mut buf = [0u8; 64];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(_) => {
                        let _ = tcsetwinsize(&master, window_size());
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        });

        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = notify_resize as *const () as usize;
        if unsafe { libc::sigaction(libc::SIGWINCH, &action, std::ptr::null_mut()) } != 0 {
            return Err(std::io::Error::last_os_error()).context("installing the SIGWINCH handler");
        }
        RESIZE_PIPE.store(write_end.as_raw_fd(), Ordering::Relaxed);

        Ok(Self {
            pipe: Some(write_end),
            thread: Some(thread),
        })
    }
}

impl Drop for ResizeForwarder {
    fn drop(&mut self) {
        // Turn the handler off before closing the pipe, or a late signal could write
        // to a descriptor number something else now owns.
        unsafe { libc::signal(libc::SIGWINCH, libc::SIG_DFL) };
        RESIZE_PIPE.store(-1, Ordering::Relaxed);
        drop(self.pipe.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

// Create the container described by `spec`, run it until it finishes, and return its exit code.
// With `tty` the container gets its own terminal, whose other end is copied to and from our stdin
// and stdout; without it it uses this process's streams. Everything the run created is deleted
// before returning.
pub fn run(spec: Spec, tty: bool) -> Result<i32> {
    let base = runtime_dir();
    let id = format!("climate-{}", store::unique_id());
    let bundle = base.join("bundles").join(&id);
    let state_root = base.join("containers");
    std::fs::create_dir_all(&bundle).with_context(|| format!("creating {}", bundle.display()))?;
    std::fs::create_dir_all(&state_root)
        .with_context(|| format!("creating {}", state_root.display()))?;
    spec.save(bundle.join("config.json"))
        .context("writing the runtime spec")?;

    // youki starts the container through a helper process that exits at once, which detaches
    // the container from us and would stop us waiting on it. Registering as a "subreaper" hands
    // such orphans back to us, not to PID 1.
    set_child_subreaper(Some(getpid())).context("becoming a child subreaper")?;
    install_signal_forwarding()?;

    // The socket must be listening before the container is built, because the container hands
    // its terminal over while it is being created.
    let console = match tty {
        true => Some(ConsoleSocket::bind(&bundle)?),
        false => None,
    };

    let mut builder = ContainerBuilder::new(id.clone(), SyscallType::default())
        .with_root_path(&state_root)
        .context("setting the container state path")?;
    if let Some(console) = &console {
        builder = builder.with_console_socket(Some(console.path()));
    }
    let mut container = builder
        .as_init(&bundle)
        .with_systemd(true)
        .with_detach(false)
        .build()
        .context("creating the container")?;

    // Collect our end of the terminal the container sent while it was created.
    let master = console.map(ConsoleSocket::into_master).transpose()?;

    let pid = container
        .pid()
        .context("container has no pid after create")?;
    let pid = Pid::from_raw(pid.as_raw()).context("container has an invalid pid")?;

    // Hand the pid to the signal handler, then deliver any signal that arrived while the container
    // was still being created.
    CONTAINER_PID.store(pid.as_raw_nonzero().get(), Ordering::Relaxed);
    let pending = PENDING_SIGNAL.swap(0, Ordering::Relaxed);
    if pending != 0 && !FORWARDED.swap(true, Ordering::Relaxed) {
        unsafe { libc::kill(pid.as_raw_nonzero().get(), pending) };
    }

    let result = (|| {
        container.start().context("starting the container")?;
        // Switch to raw mode only now that the container has started, so any error message above
        // still comes out with normal line breaks.
        let _raw = master.as_ref().map(|_| RawMode::enable()).transpose()?;
        let _resize = master.as_ref().map(ResizeForwarder::install).transpose()?;
        let pump = master.as_ref().map(pump);
        let code = wait(pid)?;
        if let Some(reader) = pump {
            let _ = reader.join();
        }
        Ok(code)
    })();

    // The container is gone and its pid may be given to another process, so stop forwarding signals
    // there. Signals arriving during the cleanup below are now swallowed, which lets it finish.
    CONTAINER_PID.store(0, Ordering::Relaxed);

    let _ = container.delete(true);
    let _ = std::fs::remove_dir_all(&bundle);
    drop(master);

    result
}

// An empty path to create in the stub layer for something to be mounted onto.
pub struct MountPoint {
    path: PathBuf,
    is_file: bool,
}

impl MountPoint {
    pub fn dir(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            is_file: false,
        }
    }

    pub fn file(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            is_file: true,
        }
    }
}

// Build the stub layer. A directory mount needs an empty directory to mount onto and a file mount
// an empty file, so each entry is created as its kind.
fn materialise_stub(stub: &Path, mountpoints: &[MountPoint]) -> Result<()> {
    for point in mountpoints {
        let relative = point.path.strip_prefix("/").unwrap_or(&point.path);
        let target = stub.join(relative);
        if point.is_file {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            File::create(&target).with_context(|| format!("creating {}", target.display()))?;
        } else {
            std::fs::create_dir_all(&target)
                .with_context(|| format!("creating {}", target.display()))?;
        }
    }
    Ok(())
}

// A layer path as a string. Layer paths are passed to fuse-overlayfs as one ':'-separated list,
// so a path containing ':' cannot be expressed.
fn lowerdir_arg(path: &Path) -> Result<String> {
    let path = path
        .to_str()
        .ok_or_else(|| anyhow!("path {} is not valid UTF-8", path.display()))?;
    if path.contains(':') {
        bail!("path '{path}' contains a ':' that overlayfs cannot express");
    }
    Ok(path.to_string())
}

// A directory of this run's own, where the assembled root filesystem lives.
fn run_dir() -> PathBuf {
    runtime_dir().join("overlays").join(store::unique_id())
}

// The container's root filesystem: the image's layers stacked by fuse-overlayfs into one directory
// tree. No writable layer is added, so the result rejects all writes and nothing has to be copied
// per run. The layers come from the shared store; only the small stub layer with the missing mount
// points is built here.
pub struct Mount {
    dir: PathBuf,
    merged: PathBuf,
}

impl Mount {
    pub fn root(&self) -> &Path {
        &self.merged
    }

    // Stack the image's layers. `layers` lists them bottom-up, the way the image stores them, while
    // fuse-overlayfs expects the topmost first, so the order is reversed. The stub goes above
    // all of them.
    pub fn new(layers: &[String], mountpoints: &[MountPoint]) -> Result<Self> {
        if layers.is_empty() {
            bail!("image has no layers to mount");
        }

        let dir = run_dir();
        let merged = dir.join("merged");
        let stub = dir.join("stub");
        std::fs::create_dir_all(&merged)
            .with_context(|| format!("creating {}", merged.display()))?;
        materialise_stub(&stub, mountpoints)?;

        let mut lowerdirs = vec![lowerdir_arg(&stub)?];
        for digest in layers.iter().rev() {
            let path = store::layer_path(digest)?;
            if !path.is_dir() {
                bail!("layer {digest} is not extracted in the store");
            }
            lowerdirs.push(lowerdir_arg(&path)?);
        }
        let lowerdir = lowerdirs.join(":");

        let status = Command::new("fuse-overlayfs")
            .arg("-o")
            .arg(format!("lowerdir={lowerdir}"))
            .arg(&merged)
            .status()
            .context("running fuse-overlayfs (is it installed?)")?;
        if !status.success() {
            let _ = std::fs::remove_dir_all(&dir);
            bail!("fuse-overlayfs failed to mount the image layers");
        }

        Ok(Self { dir, merged })
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        match Command::new("fusermount3")
            .arg("-u")
            .arg(&self.merged)
            .status()
        {
            Ok(status) if status.success() => {
                if let Err(err) = std::fs::remove_dir_all(&self.dir) {
                    eprintln!("removing overlay {}: {err}", self.dir.display());
                }
            }
            Ok(_) => eprintln!("fusermount3 failed to unmount {}", self.merged.display()),
            Err(err) => eprintln!(
                "running fusermount3 to unmount {}: {err}",
                self.merged.display()
            ),
        }
    }
}

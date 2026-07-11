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

// Per-run state lives under $XDG_RUNTIME_DIR (falling back to the temp
// directory), namespaced under climate. Runs create overlays/, bundles/, and
// containers/ here; the `clean` command reclaims them, so the base must stay
// the same on both sides - hence this single definition.
pub fn runtime_dir() -> PathBuf {
    dirs::runtime_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("climate")
}

// The real terminal's window size, so the pty matches it (0s if unavailable).
fn window_size() -> Winsize {
    tcgetwinsize(std::io::stdin()).unwrap_or(Winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    })
}

// Copy bytes until either side closes, treating a read error (e.g. EIO when the
// pty peer is gone) as a clean end of stream.
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

// Puts the terminal into raw mode for the duration of the run and restores the
// original settings on drop.
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

// The console socket over which youki returns the master side of the pty it
// creates inside the container. With the spec's process.terminal set, youki
// makes that pty's slave the container's controlling terminal and wires the
// app's stdio to it, so the line discipline turns Ctrl-C (and Ctrl-\, Ctrl-Z)
// into signals delivered to the app - which a bare stdio dup cannot do. We
// listen here, youki connects and sends the master fd while creating the
// container, and we then copy bytes between that master and our real terminal.
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

    // Receive the pty master fd the container's init passed via SCM_RIGHTS.
    // youki connects and sends during creation; the fd stays queued in the
    // socket buffer, so accepting once the container is built is enough.
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

// Spawn the copy loops between the pty master and our real stdio: stdin ->
// master (detached, ends when we exit) and master -> stdout (joined, ends when
// the master sees EOF as the container exits).
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

// Reap children until the container init exits, returning its exit code (128 +
// signal number when it was killed). Intermediate and already-reparented
// processes are reaped and ignored along the way. Running out of children
// before observing the init's status means that status was lost (reaped
// elsewhere, or the init was never ours to wait on): an error, not a success,
// so a failed app can never masquerade as exit code 0.
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

// The container init's pid, for forward_signal to relay to (0 = none yet).
static CONTAINER_PID: AtomicI32 = AtomicI32::new(0);

// A signal that arrived before the container init existed, replayed once the
// pid is known.
static PENDING_SIGNAL: AtomicI32 = AtomicI32::new(0);

// Whether a signal was already forwarded to the container init.
static FORWARDED: AtomicBool = AtomicBool::new(false);

// Relay SIGINT/SIGTERM to the container init so it shuts down and wait()
// returns, letting the normal teardown (delete container, unmount overlay,
// remove bundle) run. Without this, a signal on a non-tty run kills this
// process directly and leaks the fuse-overlayfs mount and the container
// (a tty run is unaffected: the raw terminal turns Ctrl-C into pty bytes).
// The init is PID 1 in its own PID namespace, so the kernel discards signals
// it has no handler for; a repeated signal therefore escalates to SIGKILL,
// which a namespaced init cannot ignore. Only async-signal-safe calls are
// allowed here.
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

// The write end of the resize self-pipe (-1 = none). notify_resize only writes
// a byte here - the one async-signal-safe thing it can do - and the resize
// thread does the actual window size copy.
static RESIZE_PIPE: AtomicI32 = AtomicI32::new(-1);

extern "C" fn notify_resize(_signum: libc::c_int) {
    let fd = RESIZE_PIPE.load(Ordering::Relaxed);
    if fd >= 0 {
        unsafe { libc::write(fd, [0u8].as_ptr().cast(), 1) };
    }
}

// Forwards terminal resizes (SIGWINCH) to the pty master for the duration of
// an interactive run. Copying the real terminal's size onto the master also
// delivers SIGWINCH to the container's foreground process group, so full-screen
// apps redraw at the new size. Dropping this closes the pipe, ending the thread.
struct ResizeForwarder {
    pipe: Option<OwnedFd>,
    thread: Option<JoinHandle<()>>,
}

impl ResizeForwarder {
    fn install(master: &OwnedFd) -> Result<Self> {
        let (read_end, write_end) =
            pipe_with(PipeFlags::CLOEXEC).context("creating the resize pipe")?;
        // The handler must never block, even if the pipe somehow fills up.
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
        // Disarm the handler before closing its fd so it cannot write to a
        // recycled descriptor.
        unsafe { libc::signal(libc::SIGWINCH, libc::SIG_DFL) };
        RESIZE_PIPE.store(-1, Ordering::Relaxed);
        drop(self.pipe.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

// Create the container from `spec`, run it to completion, and return its exit
// code. When `tty` is set, youki gives the container a controlling pty and
// returns its master over a console socket, which is copied to and from our
// stdio; otherwise the container inherits this process's stdio. The bundle and
// container state are removed before returning ("--rm" behaviour).
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

    // youki forks an intermediate process that forks the container init and
    // exits, so the init is reparented. Becoming a subreaper makes it our child
    // again so we can wait on it.
    set_child_subreaper(Some(getpid())).context("becoming a child subreaper")?;
    install_signal_forwarding()?;

    // For an interactive run, listen on the console socket before building so
    // the container's init can connect and hand back the pty master while it is
    // being created.
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

    // Receive the pty master the init sent over the console socket during
    // creation; the container now holds the slave as its controlling terminal.
    let master = console.map(ConsoleSocket::into_master).transpose()?;

    let pid = container
        .pid()
        .context("container has no pid after create")?;
    let pid = Pid::from_raw(pid.as_raw()).context("container has an invalid pid")?;

    // Publish the pid to the signal handler, then replay a signal that arrived
    // while the container was still being created.
    CONTAINER_PID.store(pid.as_raw_nonzero().get(), Ordering::Relaxed);
    let pending = PENDING_SIGNAL.swap(0, Ordering::Relaxed);
    if pending != 0 && !FORWARDED.swap(true, Ordering::Relaxed) {
        unsafe { libc::kill(pid.as_raw_nonzero().get(), pending) };
    }

    let result = (|| {
        container.start().context("starting the container")?;
        // Enter raw mode only now, once the container is built and started:
        // any creation diagnostics above still print with normal newlines, and
        // the terminal stays raw for the byte-copying below until this drops.
        let _raw = master.as_ref().map(|_| RawMode::enable()).transpose()?;
        let _resize = master.as_ref().map(ResizeForwarder::install).transpose()?;
        let pump = master.as_ref().map(pump);
        let code = wait(pid)?;
        if let Some(reader) = pump {
            let _ = reader.join();
        }
        Ok(code)
    })();

    // The init is reaped, so its pid may be recycled: stop forwarding to it.
    // Signals during the teardown below are absorbed so cleanup completes.
    CONTAINER_PID.store(0, Ordering::Relaxed);

    let _ = container.delete(true);
    let _ = std::fs::remove_dir_all(&bundle);
    drop(master);

    result
}

// An empty path to materialise in the stub lowerdir as a mount target.
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

// Create the stub lowerdir holding empty mount targets. Directories are created
// outright; files get an empty regular file (with parent directories) so a file
// bind mount has something to mount onto.
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

// A lowerdir path as a string, rejecting the ':' overlayfs uses as a separator.
fn lowerdir_arg(path: &Path) -> Result<String> {
    let path = path
        .to_str()
        .ok_or_else(|| anyhow!("path {} is not valid UTF-8", path.display()))?;
    if path.contains(':') {
        bail!("path '{path}' contains a ':' that overlayfs cannot express");
    }
    Ok(path.to_string())
}

// A unique, per-run overlay directory under the runtime directory.
fn run_dir() -> PathBuf {
    runtime_dir().join("overlays").join(store::unique_id())
}

// A read-only fuse-overlayfs mount of an image's extracted layers. There is no
// upperdir, so the merged root rejects writes (EROFS): the container rootfs is
// genuinely read-only and nothing is copied per run. The image layers are the
// shared lowerdirs from the store; a small per-run stub lowerdir supplies the
// empty mount targets (the working directory, /tmp, ...) that the image does not
// already contain, so youki can mount over them without writing to the root.
pub struct Mount {
    dir: PathBuf,
    merged: PathBuf,
}

impl Mount {
    pub fn root(&self) -> &Path {
        &self.merged
    }

    // Mount the image's layers read-only. `layers` are the layer digests in OCI
    // order (base first); overlayfs reads lowerdirs highest-priority first, so
    // they are reversed and the stub is placed first of all.
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

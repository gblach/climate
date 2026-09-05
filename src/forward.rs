use anyhow::{Context, Result, bail};
use oci_spec::runtime::{Hook, HookBuilder, HooksBuilder, Spec};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fs::{Mode, OFlags, open};
use rustix::io::retry_on_intr;
use rustix::net::{
    AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags, SocketFlags, SocketType, accept, bind, connect, listen,
    recvmsg, sendmsg, socket, socketpair, sockopt,
};
use rustix::pipe::{PipeFlags, pipe_with};
use std::collections::HashSet;
use std::io::{IoSlice, IoSliceMut};
use std::net::{Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpStream};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

// Two-way loopback forwarding for `network = "localhost"`.
//
// The container has a network namespace of its own, so its 127.0.0.1 is its own and neither side
// can reach the other's services. This bridges the two loopbacks: a port the host listens on is
// mirrored inside the container, a port the container listens on is mirrored on the host. Which
// ports those are is found by scanning, so no app has to list them.
//
// A socket belongs to the network namespace it was created in and goes on belonging to it, so a
// listener made inside the container still accepts connections when it is used from out here.
// Making one there takes a process that has joined the namespace, and two rules stand in the way:
//
//   - The container's namespaces cannot be reached through /proc. Its first process ends up
//     non-dumpable, which leaves /proc/<pid>/ns owned by root and shut to us. So a runtime hook,
//     which runs inside those namespaces while the container is created, opens them from the
//     inside and passes them out (see `hook`).
//   - A user namespace may only be joined while a process is single-threaded, which this one is
//     not once it is running an app. So a helper does the joining: this same binary, re-run with
//     HELPER_ARG, which hands its sockets back over a socket pair (see `helper`).

// Marker arguments this binary passes to itself for those two jobs.
pub const HOOK_ARG: &str = "__lo-hook";
pub const HELPER_ARG: &str = "__lo-helper";

// The socket the hook reports back on, in the container's bundle directory.
const HOOK_SOCKET: &str = "loopback.sock";

// The descriptors the helper finds its socket pair and the two namespaces on.
const HELPER_FD: RawFd = 3;
const USER_NS_FD: RawFd = 4;
const NET_NS_FD: RawFd = 5;

// What the parent asks the helper for: a listening socket, or a connection to a container port.
const OP_LISTEN: u8 = b'L';
const OP_CONNECT: u8 = b'C';

// How often both sides are re-read for listening ports. A service started after the container
// becomes reachable within this long.
const SCAN_INTERVAL: Timespec = Timespec {
    tv_sec: 1,
    tv_nsec: 0,
};

// Connections the kernel may queue on a mirrored port before it starts refusing them.
const BACKLOG: i32 = 128;

// Send a reply, with descriptors if there are any. A descriptor cannot travel through a normal
// write, so it rides along as ancillary data; the byte in the message itself is there because a
// zero-length message would be indistinguishable from the socket closing.
fn send_fds(sock: impl AsFd, fds: &[BorrowedFd<'_>]) -> Result<()> {
    let mut space =
        [const { std::mem::MaybeUninit::<u8>::uninit() }; rustix::cmsg_space!(ScmRights(2))];
    let mut ancillary = SendAncillaryBuffer::new(&mut space);
    if !fds.is_empty() {
        ancillary.push(SendAncillaryMessage::ScmRights(fds));
    }
    let count = [fds.len() as u8];
    retry_on_intr(|| {
        sendmsg(
            sock.as_fd(),
            &[IoSlice::new(&count)],
            &mut ancillary,
            SendFlags::empty(),
        )
    })
    .context("sending descriptors")?;
    Ok(())
}

// The container's side of the bridge, run as a createContainer hook and so from inside the
// container's namespaces. It gets the loopback interface working and passes the namespaces
// themselves out to the waiting run, which is the only way they can be had.
pub fn hook(socket: &str) -> Result<()> {
    bring_loopback_up()?;
    let stream = UnixStream::connect(socket)
        .with_context(|| format!("connecting to {socket} from inside the container"))?;
    let mut namespaces = Vec::new();
    for name in ["user", "net"] {
        let path = format!("/proc/self/ns/{name}");
        namespaces.push(
            open(&path, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())
                .with_context(|| format!("opening {path}"))?,
        );
    }
    let borrowed: Vec<BorrowedFd<'_>> = namespaces.iter().map(OwnedFd::as_fd).collect();
    send_fds(&stream, &borrowed)
}

// Enable the container's loopback interface so connections to 127.0.0.1 work. It exists in a fresh
// network namespace but starts out disabled, and switching it on from inside needs no privileges.
// The ioctl calls read its flags and set the "up" bit.
fn bring_loopback_up() -> Result<()> {
    let sock = socket(AddressFamily::INET, SocketType::DGRAM, None)
        .context("opening a socket to configure loopback")?;

    let mut req: libc::ifreq = unsafe { std::mem::zeroed() };
    for (slot, byte) in req.ifr_name.iter_mut().zip(b"lo") {
        *slot = *byte as libc::c_char;
    }

    let fd = sock.as_raw_fd();
    unsafe {
        if libc::ioctl(fd, libc::SIOCGIFFLAGS, &mut req) < 0 {
            return Err(std::io::Error::last_os_error()).context("reading loopback flags");
        }
        req.ifr_ifru.ifru_flags |= libc::IFF_UP as libc::c_short;
        if libc::ioctl(fd, libc::SIOCSIFFLAGS, &mut req) < 0 {
            return Err(std::io::Error::last_os_error()).context("bringing loopback up");
        }
    }
    Ok(())
}

// Join the namespaces the hook passed out. The user namespace has to come first: entering a network
// namespace needs CAP_SYS_ADMIN both here and in the user namespace that owns it, and joining the
// container's user namespace grants both. It is allowed because the container is this user's own.
fn enter_namespaces() -> Result<()> {
    for (fd, kind, name) in [
        (USER_NS_FD, libc::CLONE_NEWUSER, "user"),
        (NET_NS_FD, libc::CLONE_NEWNET, "network"),
    ] {
        if unsafe { libc::setns(fd, kind) } < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("joining the container's {name} namespace"));
        }
    }
    Ok(())
}

// The two addresses a port on loopback can be reached at. Both are used everywhere, because
// /proc/net does not record whether an IPv6 listener also answers on IPv4, so which one a service
// is on cannot be told from the scan - only by trying.
fn loopback_addrs(port: u16) -> [SocketAddr; 2] {
    [
        SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        SocketAddr::from((Ipv6Addr::LOCALHOST, port)),
    ]
}

// An unconnected socket of the family the address belongs to.
fn loopback_socket(addr: &SocketAddr) -> Result<OwnedFd> {
    let sock = match addr {
        SocketAddr::V4(_) => socket(AddressFamily::INET, SocketType::STREAM, None)?,
        SocketAddr::V6(_) => {
            let sock = socket(AddressFamily::INET6, SocketType::STREAM, None)?;
            // Hold the IPv6 socket to IPv6 alone, so the IPv4 one can have the same port too.
            sockopt::set_ipv6_v6only(&sock, true)?;
            sock
        }
    };
    Ok(sock)
}

fn listen_on_addr(addr: &SocketAddr) -> Result<OwnedFd> {
    let sock = loopback_socket(addr)?;
    // Without this a port stays unusable for a while after the last connection through it closed.
    sockopt::set_socket_reuseaddr(&sock, true)?;
    bind(&sock, addr)?;
    listen(&sock, BACKLOG)?;
    Ok(sock)
}

// Sockets listening on loopback, in whichever network namespace the caller is in: the helper uses
// them to mirror a host port inside the container, the run to mirror a container port out here.
// One family failing is left to itself, so a machine with IPv6 turned off still gets the other.
fn listen_on(port: u16) -> Vec<OwnedFd> {
    loopback_addrs(port)
        .iter()
        .filter_map(|addr| listen_on_addr(addr).ok())
        .collect()
}

fn connect_to_addr(addr: &SocketAddr) -> Result<OwnedFd> {
    let sock = loopback_socket(addr)?;
    connect(&sock, addr)?;
    Ok(sock)
}

// A connection to loopback, again in the caller's own network namespace, and the mirror of above.
// Whichever family the service is really on answers; the other is refused at once, on loopback.
fn connect_to(port: u16) -> Option<OwnedFd> {
    loopback_addrs(port)
        .iter()
        .find_map(|addr| connect_to_addr(addr).ok())
}

// The helper process: it joins the container's namespaces and spends the run making sockets there
// on request. It forwards nothing itself, because from in there it cannot reach the host's network.
pub fn helper() -> Result<()> {
    // The end of the socket pair the run left on a known descriptor.
    let sock = unsafe { OwnedFd::from_raw_fd(HELPER_FD) };
    enter_namespaces()?;
    // An empty reply, so the run knows the namespaces were joined before it starts the app.
    send_fds(&sock, &[])?;

    loop {
        let mut request = [0u8; 3];
        let read = retry_on_intr(|| rustix::io::read(&sock, &mut request))
            .context("reading a request from the run")?;
        // The run closed the socket, so the container is over.
        if read == 0 {
            return Ok(());
        }
        let port = u16::from_le_bytes([request[1], request[2]]);
        let made = match request[0] {
            OP_LISTEN => listen_on(port),
            OP_CONNECT => connect_to(port).into_iter().collect(),
            _ => Vec::new(),
        };
        let borrowed: Vec<BorrowedFd<'_>> = made.iter().map(OwnedFd::as_fd).collect();
        send_fds(&sock, &borrowed)?;
    }
}

// Collect the descriptors sent back. An empty list is an ordinary answer from the helper - the port
// may be taken, or out of reach without privileges - while an error means the sender is gone.
fn recv_fds(sock: impl AsFd) -> Result<Vec<OwnedFd>> {
    let mut count = [0u8; 1];
    let mut iov = [IoSliceMut::new(&mut count)];
    let mut space =
        [const { std::mem::MaybeUninit::<u8>::uninit() }; rustix::cmsg_space!(ScmRights(2))];
    let mut ancillary = RecvAncillaryBuffer::new(&mut space);
    let reply =
        retry_on_intr(|| recvmsg(sock.as_fd(), &mut iov, &mut ancillary, RecvFlags::empty()))
            .context("reading a reply")?;
    if reply.bytes == 0 {
        bail!("the other side of the loopback bridge stopped");
    }
    Ok(ancillary
        .drain()
        .filter_map(|message| match message {
            RecvAncillaryMessage::ScmRights(fds) => Some(fds),
            _ => None,
        })
        .flatten()
        .collect())
}

// Ask the helper for sockets inside the container. A port it cannot bind or reach is not worth
// stopping the run for, so every failure just means no socket this time; the next scan tries again.
fn request(sock: &OwnedFd, op: u8, port: u16) -> Vec<OwnedFd> {
    let port = port.to_le_bytes();
    match rustix::io::write(sock, &[op, port[0], port[1]]) {
        Ok(_) => recv_fds(sock).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

// Whether a connection to loopback would reach a listener on this address, as /proc/net spells it:
// four or sixteen bytes of hex, each 32-bit word in the machine's own byte order, so an IPv4
// address starts at the last byte. Loopback counts, and so does the wildcard, which covers it.
fn is_loopback(addr: &str) -> bool {
    let word = |range: std::ops::Range<usize>| u32::from_str_radix(&addr[range], 16).unwrap_or(1);
    let is_v4 = |v: u32| v == 0 || v & 0xff == 127;
    match addr.len() {
        8 => is_v4(word(0..8)),
        // Only the three IPv6 forms that can carry loopback traffic: the wildcard, ::1, and an
        // IPv4 address mapped into IPv6, which is how a dual-stack listener shows up.
        32 => match [word(0..8), word(8..16), word(16..24), word(24..32)] {
            [0, 0, 0, 0] => true,
            [0, 0, 0, 0x0100_0000] => true,
            [0, 0, 0xffff_0000, v4] => is_v4(v4),
            _ => false,
        },
        _ => false,
    }
}

// The ports something is listening on in one network namespace, read through that namespace's own
// /proc/net directory. Both IP versions count, since ::1 and the IPv6 wildcard reach loopback too.
fn listening_ports(net_dir: &str) -> HashSet<u16> {
    let mut ports = HashSet::new();
    for family in ["tcp", "tcp6"] {
        let Ok(table) = std::fs::read_to_string(format!("{net_dir}/{family}")) else {
            continue;
        };
        for line in table.lines().skip(1) {
            let mut fields = line.split_whitespace().skip(1);
            let Some((addr, port)) = fields.next().and_then(|local| local.split_once(':')) else {
                continue;
            };
            // Past the remote address is the connection state, and 0A is "listening".
            if fields.nth(1) != Some("0A") || !is_loopback(addr) {
                continue;
            }
            if let Ok(port) = u16::from_str_radix(port, 16) {
                ports.insert(port);
            }
        }
    }
    ports
}

// Copy one direction of a connection, then close that half so the far side is told it finished.
fn copy_half(mut from: TcpStream, mut to: TcpStream) {
    let _ = std::io::copy(&mut from, &mut to);
    let _ = to.shutdown(Shutdown::Write);
}

// Join two connected sockets, a thread per direction. Both threads end when the connection does.
fn pump(near: TcpStream, far: TcpStream) -> Result<()> {
    let (back_near, back_far) = (near.try_clone()?, far.try_clone()?);
    std::thread::spawn(move || copy_half(near, far));
    std::thread::spawn(move || copy_half(back_far, back_near));
    Ok(())
}

// One mirrored port, on one address family, so a port reachable over both has an entry apiece. An
// outbound mirror listens inside the container and passes what it accepts to the host; an inbound
// one listens on the host and passes it into the container.
struct Mirror {
    listener: OwnedFd,
    port: u16,
    inbound: bool,
}

// Take one waiting connection off a mirror and join it to the same port on the other side. Each
// half is made in the namespace it belongs to. A connection that cannot be completed is dropped.
fn serve(mirror: &Mirror, helper: &OwnedFd) {
    let Ok(near) = accept(&mirror.listener) else {
        return;
    };
    let far = match mirror.inbound {
        true => request(helper, OP_CONNECT, mirror.port).pop(),
        false => connect_to(mirror.port),
    };
    if let Some(far) = far {
        let _ = pump(TcpStream::from(near), TcpStream::from(far));
    }
}

// Work out which ports should be mirrored now and make the set of listeners match.
//
// The mirrors are listening sockets themselves, so they turn up in the scan and have to be taken
// back out of it, or each side would look to the other like it hosts the other's services. A port
// both sides genuinely listen on is mirrored neither way: each then reaches its own service.
fn rescan(mirrors: &mut Vec<Mirror>, helper: &OwnedFd, pid: i32) {
    let ours_host: HashSet<u16> = mirrors
        .iter()
        .filter(|m| m.inbound)
        .map(|m| m.port)
        .collect();
    let ours_container: HashSet<u16> = mirrors
        .iter()
        .filter(|m| !m.inbound)
        .map(|m| m.port)
        .collect();
    // /proc/self is this process's main thread, which never leaves the host's network namespace.
    let host = &listening_ports("/proc/self/net") - &ours_host;
    let container = &listening_ports(&format!("/proc/{pid}/net")) - &ours_container;

    let outbound = &host - &container;
    let inbound = &container - &host;
    mirrors.retain(|mirror| match mirror.inbound {
        true => inbound.contains(&mirror.port),
        false => outbound.contains(&mirror.port),
    });

    for port in &outbound - &ours_container {
        for listener in request(helper, OP_LISTEN, port) {
            mirrors.push(Mirror {
                listener,
                port,
                inbound: false,
            });
        }
    }
    for port in &inbound - &ours_host {
        for listener in listen_on(port) {
            mirrors.push(Mirror {
                listener,
                port,
                inbound: true,
            });
        }
    }
}

// Keep the mirrors in step with what both sides are listening on and hand on the connections they
// receive, until the write end of `quit` is closed. The wait between scans ends early when a
// connection arrives, so scans go by the clock instead, or a busy port would cause one apiece.
fn forward_loop(helper: OwnedFd, quit: OwnedFd, pid: i32) {
    let interval = Duration::from_secs(SCAN_INTERVAL.tv_sec as u64);
    let mut mirrors = Vec::new();
    let mut due = Instant::now();
    loop {
        if Instant::now() >= due {
            rescan(&mut mirrors, &helper, pid);
            due = Instant::now() + interval;
        }

        let mut fds = vec![PollFd::new(&quit, PollFlags::IN)];
        fds.extend(
            mirrors
                .iter()
                .map(|mirror| PollFd::new(&mirror.listener, PollFlags::IN)),
        );
        if retry_on_intr(|| poll(&mut fds, Some(&SCAN_INTERVAL))).is_err() {
            return;
        }
        if !fds[0].revents().is_empty() {
            return;
        }
        for (mirror, fd) in mirrors.iter().zip(&fds[1..]) {
            if fd.revents().contains(PollFlags::IN) {
                serve(mirror, &helper);
            }
        }
    }
}

// The socket the hook reports back on. It has to be listening before the container is built,
// because the hook runs and connects while the container is being created.
pub struct Bridge {
    listener: UnixListener,
    path: PathBuf,
}

impl Bridge {
    pub fn bind(bundle: &Path) -> Result<Self> {
        let path = bundle.join(HOOK_SOCKET);
        let listener = UnixListener::bind(&path)
            .with_context(|| format!("binding loopback socket {}", path.display()))?;
        Ok(Self { listener, path })
    }

    // Add the hook that reports back to this socket. createContainer hooks are run inside the
    // container's namespaces, which is what makes them the way in.
    pub fn install_hook(&self, spec: &mut Spec) -> Result<()> {
        let exe = std::env::current_exe().context("resolving the CLImate executable")?;
        let path = self
            .path
            .to_str()
            .context("the loopback socket path is not valid UTF-8")?;
        let hook: Hook = HookBuilder::default()
            .path(exe)
            .args(vec![
                "climate".to_string(),
                HOOK_ARG.to_string(),
                path.to_string(),
            ])
            .build()
            .context("building the loopback hook")?;
        spec.set_hooks(Some(
            HooksBuilder::default()
                .create_container(vec![hook])
                .build()
                .context("building hooks")?,
        ));
        Ok(())
    }

    // Collect the container's namespaces from the hook and start forwarding. This returns only
    // once the helper is inside them, so a failure is reported before the app is started.
    pub fn start(self, pid: i32) -> Result<Forwarder> {
        let (stream, _) = self
            .listener
            .accept()
            .context("waiting for the container's loopback hook")?;
        let namespaces = recv_fds(&stream).context("receiving the container's namespaces")?;
        let [user, net] = <[OwnedFd; 2]>::try_from(namespaces)
            .ok()
            .context("the loopback hook did not pass both namespaces")?;
        Forwarder::start(user, net, pid)
    }
}

// Loopback forwarding for one run. It lasts as long as the container: dropping it stops forwarding
// and shuts the helper down.
pub struct Forwarder {
    helper: Child,
    quit: Option<OwnedFd>,
    thread: Option<JoinHandle<()>>,
}

impl Forwarder {
    fn start(user: OwnedFd, net: OwnedFd, pid: i32) -> Result<Self> {
        let (ours, theirs) = socketpair(
            AddressFamily::UNIX,
            SocketType::SEQPACKET,
            SocketFlags::CLOEXEC,
            None,
        )
        .context("creating the loopback helper's socket")?;

        let exe = std::env::current_exe().context("resolving the CLImate executable")?;
        let sources = [theirs.into_raw_fd(), user.into_raw_fd(), net.into_raw_fd()];
        let helper = unsafe {
            Command::new(exe)
                .arg(HELPER_ARG)
                .pre_exec(move || {
                    // Move each descriptor to the number the helper looks for. The copy step
                    // clears close-on-exec, and keeps one still to be moved from being clobbered.
                    let mut moved = [0; 3];
                    for (slot, source) in moved.iter_mut().zip(sources) {
                        *slot = libc::fcntl(source, libc::F_DUPFD_CLOEXEC, 10);
                        if *slot < 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                    }
                    for (source, target) in moved.iter().zip([HELPER_FD, USER_NS_FD, NET_NS_FD]) {
                        if libc::dup2(*source, target) < 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                    }
                    Ok(())
                })
                .spawn()
        }
        .context("starting the container's loopback helper")?;
        // Drop our copies, or the helper's socket would never report the run as gone.
        for source in sources {
            drop(unsafe { OwnedFd::from_raw_fd(source) });
        }

        let mut forwarder = Self {
            helper,
            quit: None,
            thread: None,
        };
        // The helper's first reply says it is inside the container's namespaces.
        recv_fds(&ours).context("waiting for the container's loopback helper")?;

        let (quit_rx, quit_tx) =
            pipe_with(PipeFlags::CLOEXEC).context("creating the forwarding shutdown pipe")?;
        forwarder.quit = Some(quit_tx);
        forwarder.thread = Some(std::thread::spawn(move || forward_loop(ours, quit_rx, pid)));
        Ok(forwarder)
    }
}

impl Drop for Forwarder {
    fn drop(&mut self) {
        // The helper goes first, so that a loop waiting on a reply is let go rather than waited
        // for; closing the pipe then wakes the loop from the wait it is in the rest of the time.
        let _ = self.helper.kill();
        drop(self.quit.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = self.helper.wait();
    }
}

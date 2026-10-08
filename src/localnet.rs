use anyhow::{Context, Result};
use ipstack::{IpStack, IpStackConfig, IpStackStream, IpStackTcpStream, IpStackUdpStream};
use rustix::fs::{Mode, OFlags, open};
use rustix::ioctl::{Opcode, Updater};
use rustix::net::netdevice::name_to_index;
use rustix::net::{AddressFamily, SocketType, socket};
use std::fmt::Write as _;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::fd::{AsFd, OwnedFd};
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll, ready};
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf, copy_bidirectional};
use tokio::net::{TcpSocket, TcpStream, UdpSocket};
use tokio::runtime::Runtime;

// LAN access for `network = "localnet"`.
//
// The container's default route leads into a TUN device, so every packet it sends anywhere but
// loopback is read out here instead. A userspace TCP/IP stack turns those packets back into TCP
// connections and UDP flows, and each is relayed through an ordinary socket of the host - but only
// when the host would deliver it straight onto one of its own networks. A destination the host
// would hand to a gateway, the default one or any other, is refused, so nothing travels past one.

const TUN_NAME: &[u8] = b"tun0";

// The container's own addresses on the TUN device: one from the range set aside for benchmarking
// and a private IPv6 one, so neither is likely to hide a real device on the LAN.
const TUN_V4: Ipv4Addr = Ipv4Addr::new(198, 18, 0, 1);
const TUN_V6: Ipv6Addr = Ipv6Addr::new(0xfdc1, 0x1a7e, 0, 0, 0, 0, 0, 1);

// The TUN device's default MTU. The stack reads each packet into a buffer this size, so it must not
// be smaller than what the container sends.
const MTU: u16 = 1500;

// Route flags, as /proc/net/route and /proc/net/ipv6_route print them.
const RTF_UP: u32 = 0x1;
const RTF_GATEWAY: u32 = 0x2;
const RTF_REJECT: u32 = 0x200;

fn ifreq(name: &[u8]) -> libc::ifreq {
    let mut req: libc::ifreq = unsafe { std::mem::zeroed() };
    for (slot, byte) in req.ifr_name.iter_mut().zip(name) {
        *slot = *byte as libc::c_char;
    }
    req
}

fn sockaddr_v4(addr: Ipv4Addr) -> libc::sockaddr {
    let sin = libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: 0,
        sin_addr: libc::in_addr {
            s_addr: u32::from_ne_bytes(addr.octets()),
        },
        sin_zero: [0; 8],
    };
    unsafe { std::mem::transmute::<libc::sockaddr_in, libc::sockaddr>(sin) }
}

fn ioctl<const OPCODE: Opcode, T>(fd: impl AsFd, arg: &mut T, what: &str) -> Result<()> {
    unsafe { rustix::ioctl::ioctl(fd, Updater::<OPCODE, T>::new(arg)) }.context(what.to_string())
}

// An interface in a fresh network namespace starts out disabled, loopback included, and switching
// it on from inside needs no privileges.
pub fn bring_up(name: &[u8]) -> Result<()> {
    let sock = socket(AddressFamily::INET, SocketType::DGRAM, None)
        .context("opening a socket to configure an interface")?;
    let mut req = ifreq(name);
    ioctl::<{ libc::SIOCGIFFLAGS as Opcode }, _>(&sock, &mut req, "reading interface flags")?;
    unsafe { req.ifr_ifru.ifru_flags |= libc::IFF_UP as libc::c_short };
    ioctl::<{ libc::SIOCSIFFLAGS as Opcode }, _>(&sock, &mut req, "bringing an interface up")
}

// IPv6 is set up on its own, and a failure there is left alone, so a host with IPv6 turned off
// still gets IPv4. A /0 address makes the kernel route all of IPv6 to the device by itself.
fn add_v6_address(index: libc::c_int) -> Result<()> {
    let sock = socket(AddressFamily::INET6, SocketType::DGRAM, None)?;
    let mut req = libc::in6_ifreq {
        ifr6_addr: libc::in6_addr {
            s6_addr: TUN_V6.octets(),
        },
        ifr6_prefixlen: 0,
        ifr6_ifindex: index,
    };
    ioctl::<{ libc::SIOCSIFADDR as Opcode }, _>(&sock, &mut req, "adding the TUN IPv6 address")
}

// Create the TUN device in whatever network namespace the caller is in, which has to be the
// container's, and make it the default route there. The returned descriptor is the device's other
// end: what the container sends can be read from it, and what is written to it the container gets.
pub fn create_tun() -> Result<OwnedFd> {
    let tun = open(
        "/dev/net/tun",
        OFlags::RDWR | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .context("opening /dev/net/tun")?;
    let mut req = ifreq(TUN_NAME);
    req.ifr_ifru.ifru_flags = (libc::IFF_TUN | libc::IFF_NO_PI) as libc::c_short;
    ioctl::<{ libc::TUNSETIFF as Opcode }, _>(&tun, &mut req, "creating the TUN device")?;

    let sock = socket(AddressFamily::INET, SocketType::DGRAM, None)
        .context("opening a socket to configure the TUN device")?;
    let mut req = ifreq(TUN_NAME);
    req.ifr_ifru.ifru_addr = sockaddr_v4(TUN_V4);
    ioctl::<{ libc::SIOCSIFADDR as Opcode }, _>(&sock, &mut req, "adding the TUN IPv4 address")?;
    bring_up(TUN_NAME)?;

    let mut name = ifreq(TUN_NAME).ifr_name;
    let mut route: libc::rtentry = unsafe { std::mem::zeroed() };
    route.rt_dst = sockaddr_v4(Ipv4Addr::UNSPECIFIED);
    route.rt_genmask = sockaddr_v4(Ipv4Addr::UNSPECIFIED);
    route.rt_flags = RTF_UP as libc::c_ushort;
    route.rt_dev = name.as_mut_ptr();
    ioctl::<{ libc::SIOCADDRT as Opcode }, _>(&sock, &mut route, "adding the default route")?;

    let index =
        name_to_index(&sock, str::from_utf8(TUN_NAME)?).context("looking up the TUN device")?;
    let _ = add_v6_address(index as libc::c_int);
    Ok(tun)
}

// One entry of the host's routing table.
struct Route {
    net: IpAddr,
    prefix: u32,
    gateway: Option<IpAddr>,
    reject: bool,
    device: String,
}

// Whether two addresses agree in their first `prefix` bits. Shifting a whole width out cannot be
// done, but leaves nothing to compare, so a /0 matches everything.
fn same_prefix(a: u128, b: u128, width: u32, prefix: u32) -> bool {
    (a ^ b).checked_shr(width - prefix).unwrap_or(0) == 0
}

impl Route {
    fn contains(&self, ip: IpAddr) -> bool {
        match (self.net, ip) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                same_prefix(u32::from(net).into(), u32::from(ip).into(), 32, self.prefix)
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                same_prefix(net.into(), ip.into(), 128, self.prefix)
            }
            _ => false,
        }
    }

    fn on_link(&self) -> bool {
        self.gateway.is_none() && !self.reject && self.prefix > 0 && self.device != "lo"
    }
}

fn hex32(field: &str) -> Option<u32> {
    u32::from_str_radix(field, 16).ok()
}

// Addresses in /proc/net/route are 32-bit words printed in the machine's own byte order.
fn v4_route(line: &str) -> Option<Route> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    let v4 = |field: &str| Some(IpAddr::V4(Ipv4Addr::from(hex32(field)?.to_ne_bytes())));
    let flags = hex32(fields.get(3)?)?;
    if flags & RTF_UP == 0 {
        return None;
    }
    Some(Route {
        net: v4(fields.get(1)?)?,
        prefix: hex32(fields.get(7)?)?.count_ones(),
        gateway: match flags & RTF_GATEWAY != 0 {
            true => v4(fields.get(2)?),
            false => None,
        },
        reject: flags & RTF_REJECT != 0,
        device: fields.first()?.to_string(),
    })
}

// Addresses in /proc/net/ipv6_route are 32 hex digits in network order, with no separators.
fn v6_route(line: &str) -> Option<Route> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    let v6 = |field: &str| {
        let bits = u128::from_str_radix(field, 16).ok()?;
        Some(IpAddr::V6(Ipv6Addr::from(bits)))
    };
    let flags = hex32(fields.get(8)?)?;
    if flags & RTF_UP == 0 {
        return None;
    }
    Some(Route {
        net: v6(fields.first()?)?,
        prefix: hex32(fields.get(1)?)?,
        gateway: match flags & RTF_GATEWAY != 0 {
            true => v6(fields.get(4)?),
            false => None,
        },
        reject: flags & RTF_REJECT != 0,
        device: fields.get(9)?.to_string(),
    })
}

// Only the main routing table: /proc shows no other for IPv4. Policy routing may still pick
// a route from another table, but only one on the same interface, as each socket is held to it
// (see relay_tcp).
fn routes() -> Vec<Route> {
    let read = |path: &str| std::fs::read_to_string(path).unwrap_or_default();
    let v4 = read("/proc/net/route");
    let v6 = read("/proc/net/ipv6_route");
    v4.lines()
        .skip(1)
        .filter_map(v4_route)
        .chain(v6.lines().filter_map(v6_route))
        .collect()
}

fn is_unicast(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => !(ip.is_unspecified() || ip.is_broadcast() || ip.is_multicast()),
        IpAddr::V6(ip) => !(ip.is_unspecified() || ip.is_multicast()),
    }
}

// The interface the host reaches `ip` on directly, or None when it would send it through
// a gateway, or not at all. The routing table is read afresh each time, so a network the host
// joins or leaves counts from its next connection on. The kernel picks the most specific route,
// so only those count; where several are equally specific and any of them has a gateway, the
// kernel might take that one, so the address is refused.
fn egress(ip: IpAddr) -> Option<String> {
    if !is_unicast(ip) {
        return None;
    }
    let matching: Vec<Route> = routes()
        .into_iter()
        .filter(|route| route.contains(ip))
        .collect();
    let longest = matching.iter().map(|route| route.prefix).max()?;
    let best: Vec<Route> = matching
        .into_iter()
        .filter(|route| route.prefix == longest)
        .collect();
    match best.iter().all(Route::on_link) {
        true => best.into_iter().next().map(|route| route.device),
        false => None,
    }
}

// The container's /etc/resolv.conf: the host's default gateways as name servers, since a home
// router usually is one, and the host's own search domains. A link-local gateway is left out, as
// it only means something together with an interface the container does not have.
pub fn resolv_conf() -> String {
    let mut text = String::new();
    for route in routes() {
        let Some(gateway) = route.gateway else {
            continue;
        };
        let link_local = matches!(gateway, IpAddr::V6(ip) if ip.is_unicast_link_local());
        if route.prefix == 0 && !link_local {
            let _ = writeln!(text, "nameserver {gateway}");
        }
    }
    let host = std::fs::read_to_string("/etc/resolv.conf").unwrap_or_default();
    for line in host.lines() {
        if line.starts_with("search") || line.starts_with("domain") {
            let _ = writeln!(text, "{line}");
        }
    }
    text
}

// The TUN device the way the stack takes it: a stream where each read and each write is one packet.
struct Tun(AsyncFd<OwnedFd>);

impl AsyncRead for Tun {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            let mut guard = ready!(self.0.poll_read_ready(cx))?;
            let unfilled = buf.initialize_unfilled();
            match guard.try_io(|fd| Ok(rustix::io::read(fd.get_ref(), &mut *unfilled)?)) {
                Ok(read) => {
                    buf.advance(read?);
                    return Poll::Ready(Ok(()));
                }
                Err(_would_block) => continue,
            }
        }
    }
}

impl AsyncWrite for Tun {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        loop {
            let mut guard = ready!(self.0.poll_write_ready(cx))?;
            match guard.try_io(|fd| Ok(rustix::io::write(fd.get_ref(), buf)?)) {
                Ok(written) => return Poll::Ready(written),
                Err(_would_block) => continue,
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

async fn connect_tcp(target: SocketAddr) -> io::Result<TcpStream> {
    let device = egress(target.ip()).ok_or(io::ErrorKind::PermissionDenied)?;
    let socket = match target {
        SocketAddr::V4(_) => TcpSocket::new_v4()?,
        SocketAddr::V6(_) => TcpSocket::new_v6()?,
    };
    // Held to the interface the check approved, so a route that changes before the connection is
    // made cannot send it out another way.
    socket.bind_device(Some(device.as_bytes()))?;
    socket.connect(target).await
}

async fn relay_tcp(mut inside: IpStackTcpStream) -> io::Result<()> {
    match connect_tcp(inside.peer_addr()).await {
        Ok(mut outside) => {
            copy_bidirectional(&mut inside, &mut outside).await?;
        }
        // The stack has already answered the container's handshake, and dropping the connection
        // sends it nothing, so it would hang until it gave up. Closing it at least ends it now.
        Err(_) => inside.shutdown().await?,
    }
    Ok(())
}

// A flow ends once the stack has seen no packet from the container for a while.
async fn relay_udp(mut inside: IpStackUdpStream) -> io::Result<()> {
    let target = inside.peer_addr();
    let Some(device) = egress(target.ip()) else {
        return Ok(());
    };
    let any: SocketAddr = match target {
        SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
        SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    let outside = UdpSocket::bind(any).await?;
    outside.bind_device(Some(device.as_bytes()))?;
    outside.connect(target).await?;

    let mut up = vec![0u8; u16::MAX as usize];
    let mut down = vec![0u8; u16::MAX as usize];
    loop {
        tokio::select! {
            read = inside.read(&mut up) => {
                let read = read?;
                if read == 0 {
                    return Ok(());
                }
                outside.send(&up[..read]).await?;
            }
            received = outside.recv(&mut down) => {
                inside.write_all(&down[..received?]).await?;
            }
        }
    }
}

async fn relay(tun: Tun) {
    let mut config = IpStackConfig::default();
    config.mtu_unchecked(MTU);
    let mut stack = IpStack::new(config, tun);
    while let Ok(stream) = stack.accept().await {
        match stream {
            IpStackStream::Tcp(tcp) => {
                tokio::spawn(relay_tcp(tcp));
            }
            IpStackStream::Udp(udp) => {
                tokio::spawn(relay_udp(udp));
            }
            // Anything else, ICMP included, has nowhere to go.
            _ => {}
        }
    }
}

// Start relaying what the container sends into `tun`. It goes on until the returned runtime is
// dropped. The stack needs a runtime with more than one thread, as closing a TCP connection waits
// on another task.
pub fn start(tun: OwnedFd) -> Result<Runtime> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting the LAN relay")?;
    let tun = {
        let _context = runtime.enter();
        Tun(AsyncFd::new(tun).context("watching the TUN device")?)
    };
    runtime.spawn(relay(tun));
    Ok(runtime)
}

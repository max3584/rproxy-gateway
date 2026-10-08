//! The `vip` sidecar's sockets (Linux): rtnetlink to read interfaces and add or
//! remove an address, a packet socket for gratuitous ARP, a raw ICMPv6 socket for
//! unsolicited neighbor advertisements, and both to hear who else announces a VIP.
//! Needs CAP_NET_ADMIN (addresses) and CAP_NET_RAW (the packets) in the node's
//! network namespace (the fleet's pods use hostNetwork).

use std::io;
use std::mem::{size_of, zeroed};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicU32, Ordering};

use super::packet::{self, Addr, Link, Mac};

fn check(r: libc::c_int) -> io::Result<libc::c_int> {
	if r < 0 { Err(io::Error::last_os_error()) } else { Ok(r) }
}

fn socket(domain: libc::c_int, kind: libc::c_int, protocol: libc::c_int) -> io::Result<OwnedFd> {
	// SAFETY: socket(2) returns a new descriptor we own, or -1
	let fd = check(unsafe { libc::socket(domain, kind | libc::SOCK_CLOEXEC, protocol) })?;
	// SAFETY: fd is a valid descriptor nobody else owns
	Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn setsockopt<T>(fd: &OwnedFd, level: libc::c_int, name: libc::c_int, value: &T) -> io::Result<()> {
	// SAFETY: value points to a T of the given size
	check(unsafe { libc::setsockopt(fd.as_raw_fd(), level, name, (value as *const T).cast(), size_of::<T>() as libc::socklen_t) })?;
	Ok(())
}

static SEQ: AtomicU32 = AtomicU32::new(1);

/// One request on a new rtnetlink socket; the messages answered (until the
/// acknowledgement or the end of a dump).
fn netlink(request: impl FnOnce(u32) -> Vec<u8>) -> io::Result<Vec<(u16, Vec<u8>)>> {
	let fd = socket(libc::AF_NETLINK, libc::SOCK_RAW, libc::NETLINK_ROUTE)?;
	let tv = libc::timeval { tv_sec: 2, tv_usec: 0 };
	setsockopt(&fd, libc::SOL_SOCKET, libc::SO_RCVTIMEO, &tv)?;
	let seq = SEQ.fetch_add(1, Ordering::Relaxed);
	let msg = request(seq);
	// SAFETY: a zeroed sockaddr_nl is the kernel's address (pid 0)
	let mut to: libc::sockaddr_nl = unsafe { zeroed() };
	to.nl_family = libc::AF_NETLINK as libc::sa_family_t;
	// SAFETY: msg and to are valid for the lengths given
	check(unsafe {
		libc::sendto(
			fd.as_raw_fd(),
			msg.as_ptr().cast(),
			msg.len(),
			0,
			(&to as *const libc::sockaddr_nl).cast(),
			size_of::<libc::sockaddr_nl>() as libc::socklen_t,
		) as libc::c_int
	})?;
	let mut out = vec![];
	let mut buf = vec![0u8; 64 * 1024];
	loop {
		// SAFETY: buf is valid for its length
		let n = unsafe { libc::recv(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
		if n < 0 {
			return Err(io::Error::last_os_error());
		}
		for m in packet::messages(&buf[..n as usize]) {
			if m.seq != seq {
				continue;
			}
			match m.kind {
				packet::NLMSG_DONE => return Ok(out),
				packet::NLMSG_ERROR => {
					return match packet::error_code(m.payload) {
						Some(0) => Ok(out),
						Some(e) => Err(io::Error::from_raw_os_error(-e)),
						None => Err(io::Error::other("a short netlink error")),
					};
				}
				k => out.push((k, m.payload.to_vec())),
			}
		}
	}
}

/// The interfaces of the node.
pub fn links() -> io::Result<Vec<Link>> {
	let msgs = netlink(|seq| packet::dump_message(packet::RTM_GETLINK, seq))?;
	Ok(msgs.iter().filter(|(k, _)| *k == packet::RTM_NEWLINK).filter_map(|(_, p)| packet::link(p)).collect())
}

/// The addresses of the node's interfaces.
pub fn addrs() -> io::Result<Vec<Addr>> {
	let msgs = netlink(|seq| packet::dump_message(packet::RTM_GETADDR, seq))?;
	Ok(msgs.iter().filter(|(k, _)| *k == packet::RTM_NEWADDR).filter_map(|(_, p)| packet::addr(p)).collect())
}

/// Adds `ip` (`/32`, `/128` without duplicate address detection) to the interface; already there is fine.
pub fn add(index: u32, ip: IpAddr) -> io::Result<()> {
	match netlink(|seq| packet::addr_message(true, seq, index, ip)) {
		Err(e) if e.raw_os_error() == Some(libc::EEXIST) => Ok(()),
		r => r.map(|_| ()),
	}
}

/// Removes `ip` from the interface; not there is fine.
pub fn remove(index: u32, ip: IpAddr) -> io::Result<()> {
	match netlink(|seq| packet::addr_message(false, seq, index, ip)) {
		Err(e) if matches!(e.raw_os_error(), Some(libc::EADDRNOTAVAIL) | Some(libc::ENOENT) | Some(libc::ENODEV)) => Ok(()),
		r => r.map(|_| ()),
	}
}

fn sockaddr_ll(index: u32, protocol: u16, dest: Option<Mac>) -> libc::sockaddr_ll {
	// SAFETY: sockaddr_ll is plain data
	let mut a: libc::sockaddr_ll = unsafe { zeroed() };
	a.sll_family = libc::AF_PACKET as u16;
	a.sll_protocol = protocol.to_be();
	a.sll_ifindex = index as i32;
	if let Some(mac) = dest {
		a.sll_halen = 6;
		a.sll_addr[..6].copy_from_slice(&mac);
	}
	a
}

/// Sends a gratuitous ARP for `ip` from `mac` on the interface.
pub fn send_garp(index: u32, mac: Mac, ip: Ipv4Addr) -> io::Result<()> {
	let fd = socket(libc::AF_PACKET, libc::SOCK_RAW, 0)?;
	let frame = packet::garp(mac, ip);
	let to = sockaddr_ll(index, packet::ETH_P_ARP, Some([0xff; 6]));
	// SAFETY: frame and to are valid for the lengths given
	check(unsafe {
		libc::sendto(
			fd.as_raw_fd(),
			frame.as_ptr().cast(),
			frame.len(),
			0,
			(&to as *const libc::sockaddr_ll).cast(),
			size_of::<libc::sockaddr_ll>() as libc::socklen_t,
		) as libc::c_int
	})?;
	Ok(())
}

fn sockaddr_in6(ip: Ipv6Addr, scope: u32) -> libc::sockaddr_in6 {
	// SAFETY: sockaddr_in6 is plain data
	let mut a: libc::sockaddr_in6 = unsafe { zeroed() };
	a.sin6_family = libc::AF_INET6 as libc::sa_family_t;
	a.sin6_addr.s6_addr = ip.octets();
	a.sin6_scope_id = scope;
	a
}

/// Sends an unsolicited neighbor advertisement for `ip` (on the interface: it must
/// have the address) to all nodes (ff02::1), hop limit 255.
pub fn send_na(index: u32, mac: Mac, ip: Ipv6Addr) -> io::Result<()> {
	let fd = socket(libc::AF_INET6, libc::SOCK_RAW, libc::IPPROTO_ICMPV6)?;
	let hops: libc::c_int = 255;
	setsockopt(&fd, libc::IPPROTO_IPV6, libc::IPV6_MULTICAST_HOPS, &hops)?;
	setsockopt(&fd, libc::IPPROTO_IPV6, libc::IPV6_UNICAST_HOPS, &hops)?;
	let ifindex = index as libc::c_int;
	setsockopt(&fd, libc::IPPROTO_IPV6, libc::IPV6_MULTICAST_IF, &ifindex)?;
	let from = sockaddr_in6(ip, 0);
	// SAFETY: from is a valid sockaddr_in6
	check(unsafe {
		libc::bind(fd.as_raw_fd(), (&from as *const libc::sockaddr_in6).cast(), size_of::<libc::sockaddr_in6>() as libc::socklen_t)
	})?;
	let p = packet::unsolicited_na(mac, ip);
	let to = sockaddr_in6(Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 1), index);
	// SAFETY: p and to are valid for the lengths given
	check(unsafe {
		libc::sendto(
			fd.as_raw_fd(),
			p.as_ptr().cast(),
			p.len(),
			0,
			(&to as *const libc::sockaddr_in6).cast(),
			size_of::<libc::sockaddr_in6>() as libc::socklen_t,
		) as libc::c_int
	})?;
	Ok(())
}

/// Hears ARP on the interface (blocking; run on a thread): calls `heard` with each
/// sender (MAC, IPv4 address) of a packet not sent by this node.
pub fn listen_arp(index: u32, mut heard: impl FnMut(Mac, IpAddr)) -> io::Result<()> {
	let fd = socket(libc::AF_PACKET, libc::SOCK_DGRAM, i32::from(packet::ETH_P_ARP.to_be()))?;
	let at = sockaddr_ll(index, packet::ETH_P_ARP, None);
	// SAFETY: at is a valid sockaddr_ll
	check(unsafe {
		libc::bind(fd.as_raw_fd(), (&at as *const libc::sockaddr_ll).cast(), size_of::<libc::sockaddr_ll>() as libc::socklen_t)
	})?;
	let mut buf = [0u8; 256];
	loop {
		// SAFETY: sockaddr_ll is plain data
		let mut from: libc::sockaddr_ll = unsafe { zeroed() };
		let mut len = size_of::<libc::sockaddr_ll>() as libc::socklen_t;
		// SAFETY: buf and from are valid for the lengths given
		let n = unsafe {
			libc::recvfrom(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0, (&mut from as *mut libc::sockaddr_ll).cast(), &mut len)
		};
		if n < 0 {
			let e = io::Error::last_os_error();
			if e.kind() == io::ErrorKind::Interrupted {
				continue;
			}
			return Err(e);
		}
		if from.sll_pkttype == libc::PACKET_OUTGOING {
			continue;
		}
		if let Some((mac, ip)) = packet::arp_sender(&buf[..n as usize]) {
			heard(mac, IpAddr::V4(ip));
		}
	}
}

/// Hears neighbor solicitations and advertisements on every interface (blocking; run
/// on a thread): calls `heard` with who says it has which IPv6 address.
pub fn listen_nd(mut heard: impl FnMut(Mac, IpAddr)) -> io::Result<()> {
	let fd = socket(libc::AF_INET6, libc::SOCK_RAW, libc::IPPROTO_ICMPV6)?;
	// ICMP6_FILTER: a set bit blocks the type; let only solicitations and advertisements through
	let mut filter = [u32::MAX; 8];
	for t in [packet::ND_NEIGHBOR_SOLICIT, packet::ND_NEIGHBOR_ADVERT] {
		filter[usize::from(t >> 5)] &= !(1u32 << (t & 31));
	}
	setsockopt(&fd, libc::IPPROTO_ICMPV6, 1, &filter)?;
	let mut buf = [0u8; 1500];
	loop {
		let mut from = sockaddr_in6(Ipv6Addr::UNSPECIFIED, 0);
		let mut len = size_of::<libc::sockaddr_in6>() as libc::socklen_t;
		// SAFETY: buf and from are valid for the lengths given
		let n = unsafe {
			libc::recvfrom(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0, (&mut from as *mut libc::sockaddr_in6).cast(), &mut len)
		};
		if n < 0 {
			let e = io::Error::last_os_error();
			if e.kind() == io::ErrorKind::Interrupted {
				continue;
			}
			return Err(e);
		}
		let source = Ipv6Addr::from(from.sin6_addr.s6_addr);
		if let Some((mac, ip)) = packet::nd_sender(&buf[..n as usize], source) {
			heard(mac, IpAddr::V6(ip));
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::process::Command;
	use std::sync::mpsc;
	use std::time::Duration;

	fn ip(args: &str) {
		let ok = Command::new("ip").args(args.split(' ')).status().is_ok_and(|s| s.success());
		assert!(ok, "ip {args}");
	}

	/// In a network namespace of its own: `unshare -rn <test binary> --ignored vip::sys`
	/// (a veth pair v0–v1; needs CAP_NET_ADMIN and CAP_NET_RAW there).
	#[test]
	#[ignore]
	fn on_a_veth_pair() {
		ip("link add v0 type veth peer name v1");
		ip("link set v0 up");
		ip("link set v1 up");
		ip("addr add 10.9.0.1/24 dev v0");
		ip("-6 addr add 2001:db8::1/64 dev v0 nodad");
		let links = links().unwrap();
		let v0 = links.iter().find(|l| l.name == "v0").unwrap().clone();
		let v1 = links.iter().find(|l| l.name == "v1").unwrap().clone();
		assert!(v0.mac.is_some());
		let vip: IpAddr = "10.9.0.10".parse().unwrap();
		let vip6: IpAddr = "2001:db8::10".parse().unwrap();
		let all = addrs().unwrap();
		assert_eq!(packet::pick_interface(&links, &all, vip, "").map(|l| l.name.as_str()), Some("v0"));
		assert_eq!(packet::pick_interface(&links, &all, vip6, "").map(|l| l.name.as_str()), Some("v0"));

		// add twice, remove twice
		add(v0.index, vip).unwrap();
		add(v0.index, vip).unwrap();
		add(v0.index, vip6).unwrap();
		let all = addrs().unwrap();
		assert!(all.contains(&Addr { index: v0.index, ip: vip, prefix: 32 }));
		assert!(all.contains(&Addr { index: v0.index, ip: vip6, prefix: 128 }));

		// what v1's side hears
		let (tx, rx) = mpsc::channel();
		let t4 = tx.clone();
		std::thread::spawn(move || listen_arp(v1.index, move |m, a| t4.send((m, a)).unwrap()));
		std::thread::spawn(move || listen_nd(move |m, a| tx.send((m, a)).unwrap()));
		std::thread::sleep(Duration::from_millis(200));
		let mac = v0.mac.unwrap();
		send_garp(v0.index, mac, "10.9.0.10".parse().unwrap()).unwrap();
		send_na(v0.index, mac, "2001:db8::10".parse().unwrap()).unwrap();
		let mut heard = vec![];
		while let Ok(h) = rx.recv_timeout(Duration::from_secs(2)) {
			heard.push(h);
			if heard.contains(&(mac, vip)) && heard.contains(&(mac, vip6)) {
				break;
			}
		}
		assert!(heard.contains(&(mac, vip)), "{heard:?}");
		assert!(heard.contains(&(mac, vip6)), "{heard:?}");

		remove(v0.index, vip).unwrap();
		remove(v0.index, vip).unwrap();
		remove(v0.index, vip6).unwrap();
		let all = addrs().unwrap();
		assert!(!all.iter().any(|a| a.ip == vip || a.ip == vip6));
	}
}

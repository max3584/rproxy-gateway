//! The packets of the `vip` sidecar, built and read without a socket (so they
//! are tested): gratuitous ARP, unsolicited neighbor advertisements (IPv6),
//! what is learned from ARP and neighbor discovery heard on the link, and the
//! rtnetlink messages that add and remove an address.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

pub type Mac = [u8; 6];

pub const ETH_P_ARP: u16 = 0x0806;
const BROADCAST: Mac = [0xff; 6];

/// A gratuitous ARP request for `ip` from `mac` (Ethernet frame, 42 bytes):
/// sender and target address are both `ip`, so every host and switch on the
/// link that knows `ip` updates it to `mac`.
pub fn garp(mac: Mac, ip: Ipv4Addr) -> [u8; 42] {
	let mut f = [0u8; 42];
	f[0..6].copy_from_slice(&BROADCAST);
	f[6..12].copy_from_slice(&mac);
	f[12..14].copy_from_slice(&ETH_P_ARP.to_be_bytes());
	// Ethernet, IPv4, 6 and 4 bytes, request
	f[14..22].copy_from_slice(&[0, 1, 0x08, 0x00, 6, 4, 0, 1]);
	f[22..28].copy_from_slice(&mac);
	f[28..32].copy_from_slice(&ip.octets());
	// target hardware address: zero
	f[38..42].copy_from_slice(&ip.octets());
	f
}

/// The sender of an ARP packet (without its Ethernet header): who says it has which address.
/// Requests and replies both tell it (a host with the address uses it as sender).
pub fn arp_sender(p: &[u8]) -> Option<(Mac, Ipv4Addr)> {
	if p.len() < 28 || p[0..6] != [0, 1, 0x08, 0x00, 6, 4] {
		return None;
	}
	let mac: Mac = p[8..14].try_into().ok()?;
	let ip = Ipv4Addr::new(p[14], p[15], p[16], p[17]);
	Some((mac, ip))
}

pub const ND_NEIGHBOR_SOLICIT: u8 = 135;
pub const ND_NEIGHBOR_ADVERT: u8 = 136;

/// An unsolicited neighbor advertisement for `target` (ICMPv6 without the IPv6
/// header; the kernel fills the checksum of a raw ICMPv6 socket): Override set,
/// Solicited not, with the target link-layer address option.
pub fn unsolicited_na(mac: Mac, target: Ipv6Addr) -> [u8; 32] {
	let mut p = [0u8; 32];
	p[0] = ND_NEIGHBOR_ADVERT;
	// flags: Override
	p[4] = 0x20;
	p[8..24].copy_from_slice(&target.octets());
	// option 2 (target link-layer address), 1 unit of 8 bytes
	p[24] = 2;
	p[25] = 1;
	p[26..32].copy_from_slice(&mac);
	p
}

/// Who says it has which IPv6 address, from a neighbor solicitation (its source
/// address and source link-layer option) or advertisement (its target and target
/// link-layer option) heard on the link (ICMPv6 without the IPv6 header).
pub fn nd_sender(p: &[u8], source: Ipv6Addr) -> Option<(Mac, Ipv6Addr)> {
	if p.len() < 24 {
		return None;
	}
	let (address, option) = match p[0] {
		ND_NEIGHBOR_ADVERT => (Ipv6Addr::from(<[u8; 16]>::try_from(&p[8..24]).ok()?), 2),
		// duplicate address detection sends from :: (nobody has the address yet)
		ND_NEIGHBOR_SOLICIT if !source.is_unspecified() => (source, 1),
		_ => return None,
	};
	let mut o = &p[24..];
	while o.len() >= 8 {
		let len = usize::from(o[1]) * 8;
		if len == 0 || len > o.len() {
			return None;
		}
		if o[0] == option && len >= 8 {
			return Some((o[2..8].try_into().ok()?, address));
		}
		o = &o[len..];
	}
	None
}

/// Who announces `ip` on the link, if it is not us (`own`).
pub fn conflict(own: &Mac, heard: Option<(Mac, IpAddr)>, ip: IpAddr) -> bool {
	matches!(heard, Some((mac, a)) if a == ip && mac != *own && mac != [0; 6])
}

// ---------------------------------------------------------------- rtnetlink

pub const NLMSG_ERROR: u16 = 2;
pub const NLMSG_DONE: u16 = 3;
pub const RTM_NEWLINK: u16 = 16;
pub const RTM_GETLINK: u16 = 18;
pub const RTM_NEWADDR: u16 = 20;
pub const RTM_DELADDR: u16 = 21;
pub const RTM_GETADDR: u16 = 22;
pub const NLM_F_REQUEST: u16 = 0x1;
pub const NLM_F_ACK: u16 = 0x4;
pub const NLM_F_ROOT: u16 = 0x100;
pub const NLM_F_MATCH: u16 = 0x200;
pub const NLM_F_DUMP: u16 = NLM_F_ROOT | NLM_F_MATCH;
pub const NLM_F_EXCL: u16 = 0x200;
pub const NLM_F_CREATE: u16 = 0x400;
const IFA_ADDRESS: u16 = 1;
const IFA_LOCAL: u16 = 2;
const IFA_FLAGS: u16 = 8;
/// No duplicate address detection: the address is usable at once.
const IFA_F_NODAD: u32 = 0x02;
const IFLA_ADDRESS: u16 = 1;
const IFLA_IFNAME: u16 = 3;
const AF_INET: u8 = 2;
const AF_INET6: u8 = 10;

fn align(n: usize) -> usize {
	(n + 3) & !3
}

fn header(buf: &mut Vec<u8>, kind: u16, flags: u16, seq: u32) {
	buf.extend_from_slice(&0u32.to_ne_bytes()); // length, set at the end
	buf.extend_from_slice(&kind.to_ne_bytes());
	buf.extend_from_slice(&flags.to_ne_bytes());
	buf.extend_from_slice(&seq.to_ne_bytes());
	buf.extend_from_slice(&0u32.to_ne_bytes()); // port id: the kernel's
}

fn attr(buf: &mut Vec<u8>, kind: u16, data: &[u8]) {
	let len = 4 + data.len();
	buf.extend_from_slice(&(len as u16).to_ne_bytes());
	buf.extend_from_slice(&kind.to_ne_bytes());
	buf.extend_from_slice(data);
	buf.resize(align(buf.len()), 0);
}

fn finish(mut buf: Vec<u8>) -> Vec<u8> {
	let len = buf.len() as u32;
	buf[0..4].copy_from_slice(&len.to_ne_bytes());
	buf
}

fn family(ip: IpAddr) -> u8 {
	if ip.is_ipv4() { AF_INET } else { AF_INET6 }
}

fn octets(ip: IpAddr) -> Vec<u8> {
	match ip {
		IpAddr::V4(a) => a.octets().to_vec(),
		IpAddr::V6(a) => a.octets().to_vec(),
	}
}

/// `RTM_NEWADDR` (add: `/32` or `/128`, IPv6 without duplicate address detection)
/// or `RTM_DELADDR` of `ip` on the interface `index`, with an acknowledgement.
pub fn addr_message(add: bool, seq: u32, index: u32, ip: IpAddr) -> Vec<u8> {
	let mut b = Vec::with_capacity(64);
	let flags = NLM_F_REQUEST | NLM_F_ACK | if add { NLM_F_CREATE | NLM_F_EXCL } else { 0 };
	header(&mut b, if add { RTM_NEWADDR } else { RTM_DELADDR }, flags, seq);
	// ifaddrmsg: family, prefix length, flags, scope (universe), index
	let prefix = if ip.is_ipv4() { 32 } else { 128 };
	b.extend_from_slice(&[family(ip), prefix, 0, 0]);
	b.extend_from_slice(&index.to_ne_bytes());
	attr(&mut b, IFA_LOCAL, &octets(ip));
	attr(&mut b, IFA_ADDRESS, &octets(ip));
	if add && ip.is_ipv6() {
		attr(&mut b, IFA_FLAGS, &IFA_F_NODAD.to_ne_bytes());
	}
	finish(b)
}

/// A dump request: `RTM_GETLINK` or `RTM_GETADDR` (every family).
pub fn dump_message(kind: u16, seq: u32) -> Vec<u8> {
	let mut b = Vec::with_capacity(32);
	header(&mut b, kind, NLM_F_REQUEST | NLM_F_DUMP, seq);
	// ifinfomsg (16 bytes) or ifaddrmsg (8 bytes), family unspecified
	b.resize(b.len() + if kind == RTM_GETLINK { 16 } else { 8 }, 0);
	finish(b)
}

/// One netlink message: its type, sequence number and payload.
#[derive(Debug, PartialEq, Eq)]
pub struct Message<'a> {
	pub kind: u16,
	pub seq: u32,
	pub payload: &'a [u8],
}

/// The messages in a buffer read from a netlink socket.
pub fn messages(mut buf: &[u8]) -> Vec<Message<'_>> {
	let mut out = vec![];
	while buf.len() >= 16 {
		let len = u32::from_ne_bytes(buf[0..4].try_into().unwrap()) as usize;
		if len < 16 || len > buf.len() {
			break;
		}
		out.push(Message {
			kind: u16::from_ne_bytes(buf[4..6].try_into().unwrap()),
			seq: u32::from_ne_bytes(buf[8..12].try_into().unwrap()),
			payload: &buf[16..len],
		});
		buf = &buf[align(len).min(buf.len())..];
	}
	out
}

/// The error of an `NLMSG_ERROR` message (0: an acknowledgement).
pub fn error_code(payload: &[u8]) -> Option<i32> {
	Some(i32::from_ne_bytes(payload.get(0..4)?.try_into().ok()?))
}

fn attrs(mut b: &[u8]) -> Vec<(u16, &[u8])> {
	let mut out = vec![];
	while b.len() >= 4 {
		let len = usize::from(u16::from_ne_bytes([b[0], b[1]]));
		if len < 4 || len > b.len() {
			break;
		}
		out.push((u16::from_ne_bytes([b[2], b[3]]), &b[4..len]));
		b = &b[align(len).min(b.len())..];
	}
	out
}

/// A network interface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
	pub index: u32,
	pub name: String,
	pub mac: Option<Mac>,
}

/// An `RTM_NEWLINK` message's interface.
pub fn link(payload: &[u8]) -> Option<Link> {
	let index = u32::from_ne_bytes(payload.get(4..8)?.try_into().ok()?);
	let mut l = Link { index, name: String::new(), mac: None };
	for (k, v) in attrs(payload.get(16..)?) {
		match k {
			IFLA_IFNAME => l.name = String::from_utf8_lossy(v).trim_end_matches('\0').to_string(),
			IFLA_ADDRESS if v.len() == 6 => l.mac = v.try_into().ok(),
			_ => {}
		}
	}
	Some(l)
}

/// An address of an interface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Addr {
	pub index: u32,
	pub ip: IpAddr,
	pub prefix: u8,
}

/// An `RTM_NEWADDR` message's address (`IFA_LOCAL`, else `IFA_ADDRESS`).
pub fn addr(payload: &[u8]) -> Option<Addr> {
	let (fam, prefix) = (*payload.first()?, *payload.get(1)?);
	let index = u32::from_ne_bytes(payload.get(4..8)?.try_into().ok()?);
	let mut local = None;
	let mut address = None;
	for (k, v) in attrs(payload.get(8..)?) {
		let ip = match (fam, v.len()) {
			(AF_INET, 4) => IpAddr::V4(Ipv4Addr::new(v[0], v[1], v[2], v[3])),
			(AF_INET6, 16) => IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(v).ok()?)),
			_ => continue,
		};
		match k {
			IFA_LOCAL => local = Some(ip),
			IFA_ADDRESS => address = Some(ip),
			_ => {}
		}
	}
	Some(Addr { index, ip: local.or(address)?, prefix })
}

/// The interface a VIP goes on: `name` if given, else the one with an address
/// whose subnet holds the VIP (not a host route, not loopback).
pub fn pick_interface<'a>(links: &'a [Link], addrs: &[Addr], vip: IpAddr, name: &str) -> Option<&'a Link> {
	if !name.is_empty() {
		return links.iter().find(|l| l.name == name);
	}
	let max = if vip.is_ipv4() { 32 } else { 128 };
	addrs
		.iter()
		.filter(|a| a.prefix < max && a.ip != vip && !a.ip.is_loopback())
		.filter(|a| crate::render::Cidr { addr: a.ip, len: a.prefix }.contains(vip))
		.max_by_key(|a| a.prefix)
		.and_then(|a| links.iter().find(|l| l.index == a.index && l.name != "lo"))
}

#[cfg(test)]
mod tests {
	use super::*;

	const MAC: Mac = [0x02, 0x42, 0xac, 0x12, 0x00, 0x03];

	#[test]
	fn garp_frame() {
		let f = garp(MAC, Ipv4Addr::new(192, 0, 2, 10));
		assert_eq!(&f[0..6], &[0xff; 6], "broadcast");
		assert_eq!(&f[6..12], &MAC);
		assert_eq!(&f[12..14], &[0x08, 0x06]);
		assert_eq!(&f[14..22], &[0, 1, 8, 0, 6, 4, 0, 1], "Ethernet/IPv4 request");
		assert_eq!(&f[22..28], &MAC, "sender MAC");
		assert_eq!(&f[28..32], &[192, 0, 2, 10], "sender IP");
		assert_eq!(&f[32..38], &[0; 6], "target MAC");
		assert_eq!(&f[38..42], &[192, 0, 2, 10], "target IP");
		// what a listener reads (without the Ethernet header)
		assert_eq!(arp_sender(&f[14..]), Some((MAC, Ipv4Addr::new(192, 0, 2, 10))));
		assert_eq!(arp_sender(&f[14..30]), None, "short");
	}

	#[test]
	fn na_packet() {
		let t: Ipv6Addr = "2001:db8::10".parse().unwrap();
		let p = unsolicited_na(MAC, t);
		assert_eq!(p[0], 136);
		assert_eq!(p[1], 0, "code");
		assert_eq!(p[4], 0x20, "Override, not Solicited, not Router");
		assert_eq!(&p[8..24], &t.octets());
		assert_eq!(&p[24..26], &[2, 1], "target link-layer address option");
		assert_eq!(&p[26..32], &MAC);
		assert_eq!(nd_sender(&p, "fe80::1".parse().unwrap()), Some((MAC, t)));
	}

	#[test]
	fn ns_sender() {
		let src: Ipv6Addr = "2001:db8::10".parse().unwrap();
		// solicitation for fe80::2 with a source link-layer address option
		let mut p = vec![135, 0, 0, 0, 0, 0, 0, 0];
		p.extend_from_slice(&"fe80::2".parse::<Ipv6Addr>().unwrap().octets());
		p.extend_from_slice(&[1, 1]);
		p.extend_from_slice(&MAC);
		assert_eq!(nd_sender(&p, src), Some((MAC, src)));
		assert_eq!(nd_sender(&p, Ipv6Addr::UNSPECIFIED), None, "duplicate address detection");
		// an option of length 0 is malformed
		let mut bad = p.clone();
		bad[25] = 0;
		assert_eq!(nd_sender(&bad, src), None);
		// no link-layer option
		assert_eq!(nd_sender(&p[..24], src), None);
	}

	#[test]
	fn conflicts() {
		let vip: IpAddr = "192.0.2.10".parse().unwrap();
		let other = [0x02, 0, 0, 0, 0, 9];
		assert!(conflict(&MAC, Some((other, vip)), vip));
		assert!(!conflict(&MAC, Some((MAC, vip)), vip), "our own announcement");
		assert!(!conflict(&MAC, Some((other, "192.0.2.11".parse().unwrap())), vip), "another address");
		assert!(!conflict(&MAC, None, vip));
		assert!(!conflict(&MAC, Some(([0; 6], vip)), vip));
	}

	#[test]
	fn addr_messages() {
		let m = addr_message(true, 7, 3, "192.0.2.10".parse().unwrap());
		assert_eq!(u32::from_ne_bytes(m[0..4].try_into().unwrap()) as usize, m.len());
		assert_eq!(u16::from_ne_bytes(m[4..6].try_into().unwrap()), RTM_NEWADDR);
		let flags = u16::from_ne_bytes(m[6..8].try_into().unwrap());
		assert_eq!(flags, NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL);
		assert_eq!(u32::from_ne_bytes(m[8..12].try_into().unwrap()), 7);
		assert_eq!(&m[16..20], &[2, 32, 0, 0], "AF_INET /32");
		assert_eq!(u32::from_ne_bytes(m[20..24].try_into().unwrap()), 3);
		// the message reads back as the address it carries
		let msgs = messages(&m);
		assert_eq!(msgs.len(), 1);
		assert_eq!(addr(msgs[0].payload), Some(Addr { index: 3, ip: "192.0.2.10".parse().unwrap(), prefix: 32 }));

		let v6 = addr_message(true, 8, 4, "2001:db8::10".parse().unwrap());
		assert_eq!(v6[16..18], [10, 128]);
		let p = messages(&v6)[0].payload;
		let flags = attrs(&p[8..]).into_iter().find(|(k, _)| *k == IFA_FLAGS).map(|(_, v)| u32::from_ne_bytes(v.try_into().unwrap()));
		assert_eq!(flags, Some(IFA_F_NODAD));

		let del = addr_message(false, 9, 4, "2001:db8::10".parse().unwrap());
		assert_eq!(u16::from_ne_bytes(del[4..6].try_into().unwrap()), RTM_DELADDR);
		assert_eq!(u16::from_ne_bytes(del[6..8].try_into().unwrap()), NLM_F_REQUEST | NLM_F_ACK);
		assert!(attrs(&messages(&del)[0].payload[8..]).iter().all(|(k, _)| *k != IFA_FLAGS));
	}

	#[test]
	fn dumps_and_links() {
		let d = dump_message(RTM_GETLINK, 1);
		assert_eq!(d.len(), 32);
		assert_eq!(u16::from_ne_bytes(d[6..8].try_into().unwrap()), NLM_F_REQUEST | NLM_F_DUMP);
		assert_eq!(dump_message(RTM_GETADDR, 1).len(), 24);
		// an RTM_NEWLINK as the kernel sends it: ifinfomsg, IFLA_IFNAME "eth0\0", IFLA_ADDRESS
		let mut b = vec![];
		header(&mut b, RTM_NEWLINK, 2, 1);
		b.extend_from_slice(&[0, 0, 1, 0]);
		b.extend_from_slice(&5u32.to_ne_bytes());
		b.extend_from_slice(&[0; 8]);
		attr(&mut b, IFLA_IFNAME, b"eth0\0");
		attr(&mut b, IFLA_ADDRESS, &MAC);
		let mut two = finish(b);
		let mut done = vec![];
		header(&mut done, NLMSG_DONE, 2, 1);
		done.extend_from_slice(&0u32.to_ne_bytes());
		two.extend(finish(done));
		let msgs = messages(&two);
		assert_eq!(msgs.iter().map(|m| m.kind).collect::<Vec<_>>(), vec![RTM_NEWLINK, NLMSG_DONE]);
		assert_eq!(link(msgs[0].payload), Some(Link { index: 5, name: "eth0".into(), mac: Some(MAC) }));
		assert_eq!(error_code(msgs[1].payload), Some(0));
	}

	#[test]
	fn interface_choice() {
		let links = vec![
			Link { index: 1, name: "lo".into(), mac: None },
			Link { index: 2, name: "eth0".into(), mac: Some(MAC) },
			Link { index: 3, name: "eth1".into(), mac: Some([2, 0, 0, 0, 0, 1]) },
		];
		let a = |index, ip: &str, prefix| Addr { index, ip: ip.parse().unwrap(), prefix };
		let addrs =
			vec![a(1, "127.0.0.1", 8), a(2, "172.18.0.3", 16), a(3, "10.0.0.5", 24), a(3, "10.0.0.99", 32), a(2, "2001:db8::3", 64)];
		let pick = |vip: &str, name| pick_interface(&links, &addrs, vip.parse().unwrap(), name).map(|l| l.name.clone());
		assert_eq!(pick("172.18.255.10", ""), Some("eth0".into()));
		assert_eq!(pick("10.0.0.10", ""), Some("eth1".into()));
		assert_eq!(pick("2001:db8::10", ""), Some("eth0".into()));
		assert_eq!(pick("192.0.2.10", ""), None, "no subnet holds it");
		assert_eq!(pick("10.0.0.99", ""), Some("eth1".into()), "itself (/32) is skipped, the /24 holds it");
		assert_eq!(pick("192.0.2.10", "eth1"), Some("eth1".into()), "named");
		assert_eq!(pick("192.0.2.10", "eth9"), None);
	}
}

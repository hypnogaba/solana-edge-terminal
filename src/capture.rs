//! Reading the DoubleZero multicast feed off the tunnel interface.
//!
//! A normal UDP multicast socket receives ZERO datagrams on `doublezero1`, even
//! with the group joined, the MULTICAST flag set and rp_filter off. `tcpdump`
//! sees the full feed, so we tap the interface the same way it does, with an
//! AF_PACKET socket, and parse IP/UDP by hand. Measured on the Kalshi feed
//! carried over the same tunnel: UDP socket 0 packets vs AF_PACKET ~3.4k
//! packets in 5 seconds on 233.84.178.3:31000.
//!
//! With `SOCK_DGRAM` the kernel strips the link-layer header, so each read
//! starts at the IPv4 header.

use std::collections::HashSet;
use std::io;
use std::mem;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
use std::thread;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use tokio::sync::mpsc::UnboundedSender;

const ETH_P_IP: u16 = 0x0800;
const IPPROTO_UDP: u8 = 17;
const RECV_BUF: usize = 65_536;

/// One captured datagram, stamped the moment it left the kernel.
#[derive(Debug, Clone)]
pub struct CapturedPacket {
    pub payload: Vec<u8>,
    pub at: Instant,
    pub from: SocketAddr,
    pub dst_port: u16,
}

/// What to keep out of everything crossing the interface.
#[derive(Debug, Clone)]
pub struct Filter {
    pub group: Ipv4Addr,
    pub ports: HashSet<u16>,
}

impl Filter {
    pub fn new(group: Ipv4Addr, ports: impl IntoIterator<Item = u16>) -> Self {
        Self { group, ports: ports.into_iter().collect() }
    }
}

/// Parse one IPv4 packet, returning the UDP payload if it is one we want.
///
/// Kept pure and separate from the socket so it can be tested against captured
/// bytes: everything that can silently corrupt a shred happens here.
pub fn parse_udp<'a>(buf: &'a [u8], filter: &Filter) -> Option<(&'a [u8], SocketAddrV4, u16)> {
    if buf.len() < 20 {
        return None;
    }
    let version = buf[0] >> 4;
    if version != 4 {
        return None;
    }
    if buf[9] != IPPROTO_UDP {
        return None;
    }
    // Fragmented datagrams would hand a partial shred downstream. Shreds are
    // ~1.2 KB and the tunnel MTU is 1476, so this should never fire; if it
    // does, dropping is right and reassembling here would be wrong.
    let frag = u16::from_be_bytes([buf[6], buf[7]]);
    if frag & 0x1FFF != 0 {
        return None;
    }
    let dst = Ipv4Addr::new(buf[16], buf[17], buf[18], buf[19]);
    if dst != filter.group {
        return None;
    }
    let ihl = ((buf[0] & 0x0F) as usize) * 4;
    if ihl < 20 || buf.len() < ihl + 8 {
        return None;
    }
    // Trust the IP header's own length, not the read size: a short datagram
    // arrives padded to the Ethernet minimum, and those pad bytes appended to a
    // shred make it undecodable.
    let total_len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    let end = total_len.min(buf.len());
    if end < ihl + 8 {
        return None;
    }
    let src_port = u16::from_be_bytes([buf[ihl], buf[ihl + 1]]);
    let dst_port = u16::from_be_bytes([buf[ihl + 2], buf[ihl + 3]]);
    if !filter.ports.contains(&dst_port) {
        return None;
    }
    let src = Ipv4Addr::new(buf[12], buf[13], buf[14], buf[15]);
    Some((&buf[ihl + 8..end], SocketAddrV4::new(src, src_port), dst_port))
}

fn open_af_packet(iface: &str) -> Result<OwnedFd> {
    let name = std::ffi::CString::new(iface).context("interface name")?;
    // SAFETY: name is a valid NUL-terminated C string for the duration of the call.
    let ifindex = unsafe { libc::if_nametoindex(name.as_ptr()) };
    if ifindex == 0 {
        bail!("interface {iface} not found: {}", io::Error::last_os_error());
    }
    // SAFETY: plain socket(2) with constant arguments.
    let fd = unsafe {
        libc::socket(
            libc::AF_PACKET,
            libc::SOCK_DGRAM,
            (ETH_P_IP.to_be()) as libc::c_int,
        )
    };
    if fd < 0 {
        bail!(
            "AF_PACKET socket failed ({}). This needs CAP_NET_RAW.",
            io::Error::last_os_error()
        );
    }
    // SAFETY: fd is a fresh, valid descriptor we now own.
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };

    // SAFETY: zeroed sockaddr_ll is a valid representation; every field we care
    // about is set below and the length matches the struct we pass.
    let mut addr: libc::sockaddr_ll = unsafe { mem::zeroed() };
    addr.sll_family = libc::AF_PACKET as u16;
    addr.sll_protocol = ETH_P_IP.to_be();
    addr.sll_ifindex = ifindex as i32;
    let rc = unsafe {
        libc::bind(
            owned.as_raw_fd(),
            &addr as *const _ as *const libc::sockaddr,
            mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t,
        )
    };
    if rc < 0 {
        bail!("bind to {iface} failed: {}", io::Error::last_os_error());
    }
    Ok(owned)
}

/// Read the interface forever on a dedicated OS thread, forwarding matches.
///
/// AF_PACKET reads block, and this is the latency-critical path, so it gets a
/// real thread rather than a place in the async runtime's queue.
pub fn spawn(
    iface: &str,
    filter: Filter,
    tx: UnboundedSender<CapturedPacket>,
) -> Result<thread::JoinHandle<()>> {
    let fd = open_af_packet(iface)?;
    let iface = iface.to_string();
    Ok(thread::Builder::new()
        .name(format!("capture-{iface}"))
        .spawn(move || {
            let mut buf = vec![0u8; RECV_BUF];
            loop {
                // SAFETY: buf is valid for RECV_BUF bytes for the call's duration.
                let n = unsafe {
                    libc::recv(fd.as_raw_fd(), buf.as_mut_ptr() as *mut libc::c_void, RECV_BUF, 0)
                };
                if n <= 0 {
                    let err = io::Error::last_os_error();
                    if err.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    tracing::error!("capture on {iface} stopped: {err}");
                    return;
                }
                let at = Instant::now();
                if let Some((payload, from, dst_port)) = parse_udp(&buf[..n as usize], &filter) {
                    let packet = CapturedPacket {
                        payload: payload.to_vec(),
                        at,
                        from: SocketAddr::V4(from),
                        dst_port,
                    };
                    if tx.send(packet).is_err() {
                        return; // receiver gone: shutting down
                    }
                }
            }
        })
        .context("spawn capture thread")?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an IPv4+UDP packet the way it arrives from AF_PACKET/SOCK_DGRAM,
    /// optionally padded the way a short frame is padded on the wire.
    fn packet(dst: Ipv4Addr, dst_port: u16, payload: &[u8], pad_to: usize) -> Vec<u8> {
        let total_len = 20 + 8 + payload.len();
        let mut buf = vec![0u8; 20];
        buf[0] = 0x45; // IPv4, IHL 5
        buf[2..4].copy_from_slice(&(total_len as u16).to_be_bytes());
        buf[9] = IPPROTO_UDP;
        buf[12..16].copy_from_slice(&Ipv4Addr::new(10, 0, 0, 1).octets());
        buf[16..20].copy_from_slice(&dst.octets());
        buf.extend_from_slice(&1234u16.to_be_bytes());
        buf.extend_from_slice(&dst_port.to_be_bytes());
        buf.extend_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
        buf.extend_from_slice(&[0, 0]); // checksum
        buf.extend_from_slice(payload);
        while buf.len() < pad_to {
            buf.push(0);
        }
        buf
    }

    fn filter() -> Filter {
        Filter::new(Ipv4Addr::new(233, 84, 178, 1), [7733])
    }

    #[test]
    fn keeps_only_the_group_and_port_we_asked_for() {
        let f = filter();
        let good = packet(Ipv4Addr::new(233, 84, 178, 1), 7733, b"shred", 0);
        assert!(parse_udp(&good, &f).is_some());

        let other_group = packet(Ipv4Addr::new(233, 84, 178, 3), 7733, b"shred", 0);
        assert!(parse_udp(&other_group, &f).is_none());

        let other_port = packet(Ipv4Addr::new(233, 84, 178, 1), 31000, b"shred", 0);
        assert!(parse_udp(&other_port, &f).is_none());
    }

    #[test]
    fn ethernet_padding_is_cut_off() {
        // A short datagram arrives padded to the 60-byte Ethernet minimum. Pad
        // bytes appended to a shred make it undecodable, and nothing downstream
        // would report why: the shred would simply fail to parse.
        let padded = packet(Ipv4Addr::new(233, 84, 178, 1), 7733, b"tiny", 60);
        assert!(padded.len() > 32, "test needs a genuinely padded frame");
        let (payload, _, _) = parse_udp(&padded, &filter()).expect("should parse");
        assert_eq!(payload, b"tiny", "payload must be cut at the IP total length");
    }

    #[test]
    fn a_fragment_is_dropped_rather_than_handed_on_half_complete() {
        let mut fragment = packet(Ipv4Addr::new(233, 84, 178, 1), 7733, b"half", 0);
        fragment[6..8].copy_from_slice(&37u16.to_be_bytes()); // non-zero offset
        assert!(parse_udp(&fragment, &filter()).is_none());
    }

    #[test]
    fn non_udp_and_truncated_packets_are_ignored() {
        let mut tcp = packet(Ipv4Addr::new(233, 84, 178, 1), 7733, b"x", 0);
        tcp[9] = 6;
        assert!(parse_udp(&tcp, &filter()).is_none());
        assert!(parse_udp(&[0x45, 0, 0], &filter()).is_none());
    }

    #[test]
    fn a_longer_ip_header_shifts_the_payload() {
        // IHL 6 (one option word). Reading the payload at a fixed offset would
        // hand the shred parser four bytes of IP option.
        let mut buf = packet(Ipv4Addr::new(233, 84, 178, 1), 7733, b"payload", 0);
        buf[0] = 0x46;
        buf.splice(20..20, [0u8; 4]);
        let total = u16::from_be_bytes([buf[2], buf[3]]) + 4;
        buf[2..4].copy_from_slice(&total.to_be_bytes());
        let (payload, _, port) = parse_udp(&buf, &filter()).expect("should parse");
        assert_eq!(payload, b"payload");
        assert_eq!(port, 7733);
    }
}

use super::view_pace::MAX_BATCH;
use std::{io, net::{SocketAddrV4, UdpSocket}, os::fd::AsRawFd};

pub(super) fn send_view_packets(socket: &UdpSocket, peer: Option<SocketAddrV4>, packets: &[&[u8]])
    -> io::Result<usize>
{
    debug_assert!((1..=MAX_BATCH).contains(&packets.len()));
    let mut iov: [libc::iovec; MAX_BATCH] = unsafe { std::mem::zeroed() };
    let mut messages: [libc::mmsghdr; MAX_BATCH] = unsafe { std::mem::zeroed() };
    let address = peer.map(|peer| libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: peer.port().to_be(),
        sin_addr: libc::in_addr { s_addr: u32::from_ne_bytes(peer.ip().octets()) },
        sin_zero: [0; 8],
    });
    for (index, packet) in packets.iter().enumerate() {
        iov[index].iov_base = packet.as_ptr().cast_mut().cast();
        iov[index].iov_len = packet.len();
        messages[index].msg_hdr.msg_iov = &mut iov[index];
        messages[index].msg_hdr.msg_iovlen = 1;
        if let Some(address) = address.as_ref() {
            messages[index].msg_hdr.msg_name = std::ptr::from_ref(address).cast_mut().cast();
            messages[index].msg_hdr.msg_namelen = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
        }
    }
    let sent = unsafe {
        libc::sendmmsg(socket.as_raw_fd(), messages.as_mut_ptr(), packets.len() as libc::c_uint, libc::MSG_DONTWAIT as _)
    };
    if sent < 0 { Err(io::Error::last_os_error()) } else { Ok(sent as usize) }
}

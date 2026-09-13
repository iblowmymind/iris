//! DNS work is isolated from the NAT loop so resolver timeouts cannot stall TCP/NFS.
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::time::Duration;

pub(crate) struct Request {
    pub client_mac: [u8; 6],
    pub client_ip: Ipv4Addr,
    pub server_ip: Ipv4Addr,
    pub client_port: u16,
    pub query: Vec<u8>,
}

pub(crate) struct DnsProxy {
    requests: SyncSender<Request>,
    replies: Receiver<(Request, Vec<u8>)>,
}

impl DnsProxy {
    pub fn new(upstream: Option<SocketAddr>) -> std::io::Result<Self> {
        let (requests, incoming) = mpsc::sync_channel::<Request>(32);
        let (outgoing, replies) = mpsc::sync_channel(32);
        std::thread::Builder::new()
            .name("nat-dns".into())
            .spawn(move || {
                while let Ok(request) = incoming.recv() {
                    let reply = match upstream {
                        Some(server) => udp_query(server, &request.query),
                        None => system_query(&request.query),
                    }
                    .or_else(|| error_reply(&request.query, 2)); // SERVFAIL
                    if let Some(reply) = reply {
                        if outgoing.send((request, reply)).is_err() {
                            break;
                        }
                    }
                }
            })?;
        Ok(Self { requests, replies })
    }

    pub fn submit(&self, request: Request) {
        // Bound memory use. DNS clients retry if the queue is full.
        let _ = self.requests.try_send(request);
    }

    pub fn poll(&self) -> Option<(Request, Vec<u8>)> {
        self.replies.try_recv().ok()
    }
}

fn udp_query(server: SocketAddr, query: &[u8]) -> Option<Vec<u8>> {
    if query.len() < 12 {
        return None;
    }
    let bind = if server.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let socket = UdpSocket::bind(bind).ok()?;
    socket.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    socket.connect(server).ok()?; // Accept replies only from this resolver.
    socket.send(query).ok()?;
    let mut reply = vec![0; 512];
    let n = socket.recv(&mut reply).ok()?;
    if n < 12 || reply[..2] != query[..2] || reply[2] & 0x80 == 0 {
        return None;
    }
    reply.truncate(n);
    Some(reply)
}

// Ordinary, single-question DNS queries emitted by IRIX. Reject compression and
// malformed labels rather than letting guest bytes reach a C string unchecked.
fn question(query: &[u8]) -> Option<(String, u16, u16, usize)> {
    if query.len() < 12 || query[2] & 0xf8 != 0 || query[4..6] != [0, 1] {
        return None;
    }
    let mut pos = 12;
    let mut name = String::new();
    loop {
        let len = *query.get(pos)? as usize;
        pos += 1;
        if len == 0 {
            break;
        }
        if len > 63 || pos + len - 12 > 254 {
            return None;
        }
        for &byte in query.get(pos..pos + len)? {
            if byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_' {
                name.push(byte as char);
            } else {
                name.push_str(&format!("\\{:03}", byte));
            }
        }
        name.push('.');
        pos += len;
    }
    if name.is_empty() {
        name.push('.');
    }
    let tail = query.get(pos..pos + 4)?;
    Some((
        name,
        u16::from_be_bytes([tail[0], tail[1]]),
        u16::from_be_bytes([tail[2], tail[3]]),
        pos + 4,
    ))
}

fn error_reply(query: &[u8], rcode: u8) -> Option<Vec<u8>> {
    let (_, _, _, end) = question(query)?;
    let mut reply = query[..end].to_vec();
    reply[2] = 0x80 | (query[2] & 1); // QR and original RD
    reply[3] = 0x80 | (rcode & 15); // RA
    reply[6..12].fill(0);
    Some(reply)
}

#[cfg(target_os = "macos")]
fn system_query(query: &[u8]) -> Option<Vec<u8>> {
    macos_query(query, None)
}

#[cfg(target_os = "macos")]
fn macos_query(query: &[u8], config: Option<&std::ffi::CStr>) -> Option<Vec<u8>> {
    use std::ffi::{c_char, c_void, CString};
    #[link(name = "resolv")]
    extern "C" {
        fn dns_open(name: *const c_char) -> *const c_void;
        fn dns_free(handle: *const c_void);
        fn dns_query(
            handle: *const c_void,
            name: *const c_char,
            class: u32,
            kind: u32,
            buf: *mut c_char,
            len: u32,
            from: *mut libc::sockaddr,
            fromlen: *mut u32,
        ) -> i32;
    }
    let (name, kind, class, _) = question(query)?;
    let name = CString::new(name).ok()?;
    let mut reply = vec![0u8; 512];
    // dns_open(NULL) selects macOS's Super resolver, including VPN and domain
    // configurations. Open per query to observe network changes without restart.
    // SAFETY: the handle stays on this worker and is freed once; buffers and
    // sockaddr storage remain valid for the synchronous call (SDK dns.h ABI).
    let n = unsafe {
        let handle = dns_open(config.map_or(std::ptr::null(), |path| path.as_ptr()));
        if handle.is_null() {
            return None;
        }
        let mut from: libc::sockaddr_storage = std::mem::zeroed();
        let mut fromlen = std::mem::size_of_val(&from) as u32;
        let n = dns_query(
            handle,
            name.as_ptr(),
            class as u32,
            kind as u32,
            reply.as_mut_ptr().cast(),
            reply.len() as u32,
            (&mut from as *mut libc::sockaddr_storage).cast(),
            &mut fromlen,
        );
        dns_free(handle);
        n
    };
    if n < 12 {
        // libresolv can return -1 for NXDOMAIN/NODATA while leaving the DNS
        // response header in the buffer. Preserve that result, not SERVFAIL.
        return if reply[2] & 0x80 != 0 {
            error_reply(query, reply[3] & 15)
        } else {
            None
        };
    }
    if n as usize > reply.len() {
        // Return a well-formed truncated response rather than partial records.
        let mut truncated = error_reply(query, 0)?;
        truncated[2] |= 2;
        return Some(truncated);
    }
    reply.truncate(n as usize);
    reply[..2].copy_from_slice(&query[..2]);
    Some(reply)
}

#[cfg(not(target_os = "macos"))]
fn system_query(_query: &[u8]) -> Option<Vec<u8>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    fn query(kind: u16) -> Vec<u8> {
        let mut q = vec![0x12, 0x34, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0];
        q.extend_from_slice(b"\x07example\x03com\0");
        q.extend_from_slice(&kind.to_be_bytes());
        q.extend_from_slice(&[0, 1]);
        q
    }
    #[test]
    fn parses_names_and_record_types() {
        for kind in [1, 12, 15, 28, 16] {
            let q = query(kind);
            assert_eq!(
                question(&q),
                Some(("example.com.".into(), kind, 1, q.len()))
            );
        }
    }
    #[test]
    fn rejects_malformed_questions() {
        let q = query(1);
        for end in 0..q.len() {
            assert!(question(&q[..end]).is_none());
        }
        let mut q = q;
        q[12] = 0xc0;
        assert!(question(&q).is_none());
        q[12] = 64;
        assert!(question(&q).is_none());
    }
    #[test]
    fn negative_reply_preserves_question_and_id() {
        let q = query(12);
        for code in [0, 2, 3] {
            let r = error_reply(&q, code).unwrap();
            assert_eq!(&r[..2], &q[..2]);
            assert_eq!(&r[12..], &q[12..]);
            assert_eq!(r[2], 0x81);
            assert_eq!(r[3], 0x80 | code);
            assert_eq!(&r[6..12], &[0; 6]);
        }
    }
    #[test]
    fn forwards_to_explicit_resolver_on_worker() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let proxy = DnsProxy::new(Some(server.local_addr().unwrap())).unwrap();
        let q = query(1);
        proxy.submit(Request {
            client_mac: [0; 6],
            client_ip: Ipv4Addr::LOCALHOST,
            server_ip: Ipv4Addr::new(8, 8, 8, 8),
            client_port: 1234,
            query: q.clone(),
        });
        let mut buf = [0; 512];
        let (n, peer) = server.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], &q);
        let expected = error_reply(&q, 3).unwrap();
        server.send_to(&expected, peer).unwrap();
        let (request, reply) = proxy.replies.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(reply, expected);
        assert_eq!(request.server_ip, Ipv4Addr::new(8, 8, 8, 8));
        assert_eq!(request.client_port, 1234);
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn native_resolver_preserves_answers_and_negative_results() {
        // A private resolver file exercises libresolv without changing host DNS
        // or making external requests. Each case uses a different server/handle.
        for rcode in [0, 3] {
            let server = UdpSocket::bind("127.0.0.1:0").unwrap();
            server
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let port = server.local_addr().unwrap().port();
            let path =
                std::env::temp_dir().join(format!("iris-dns-{}-{port}.conf", std::process::id()));
            std::fs::write(
                &path,
                format!("port {port}\nnameserver 127.0.0.1\ntimeout 1\noptions attempts:1\n"),
            )
            .unwrap();
            let responder = std::thread::spawn(move || {
                let mut buf = [0; 512];
                let (n, peer) = server.recv_from(&mut buf).unwrap();
                let mut response = error_reply(&buf[..n], rcode).unwrap();
                if rcode == 0 {
                    response[7] = 1;
                    response.extend_from_slice(&[
                        0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 192, 0, 2, 1,
                    ]);
                }
                server.send_to(&response, peer).unwrap();
            });
            let config = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
            let q = query(1);
            let response = macos_query(&q, Some(&config));
            std::fs::remove_file(path).unwrap();
            responder.join().unwrap();
            let response = response.expect("native DNS reply");
            assert_eq!(&response[..2], &q[..2]);
            assert_eq!(response[3] & 15, rcode);
            if rcode == 0 {
                assert_eq!(&response[response.len() - 4..], &[192, 0, 2, 1]);
            }
        }
    }
}

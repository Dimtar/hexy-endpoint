//! Asks the server which shares it offers, over SMB itself. This works with
//! any account that can log in to the shares, not just a HexOS admin.
//!
//! The share list comes from the `srvsvc` RPC service on the server's IPC$
//! pipe (the same call Windows Explorer makes). The `smb` crate's own RPC
//! client only speaks the NDR64 encoding, which Samba (and so HexOS) rejects,
//! so the one call needed here, NetrShareEnum, is encoded by hand in classic
//! NDR, which every SMB server accepts.

use std::time::Duration;

#[derive(Debug, PartialEq)]
pub struct RemoteShare {
    pub name: String,
    pub comment: String,
}

const TIMEOUT: Duration = Duration::from_secs(20);

pub async fn list_shares(
    host: &str,
    username: &str,
    password: &str,
) -> Result<Vec<RemoteShare>, String> {
    list_shares_on_port(host, 0, username, password).await
}

/// `port` 0 means the standard SMB port.
pub async fn list_shares_on_port(
    host: &str,
    port: u16,
    username: &str,
    password: &str,
) -> Result<Vec<RemoteShare>, String> {
    match tokio::time::timeout(TIMEOUT, query(host, port, username, password)).await {
        Ok(result) => result,
        Err(_) => Err(format!("{host} did not respond")),
    }
}

async fn query(
    host: &str,
    port: u16,
    username: &str,
    password: &str,
) -> Result<Vec<RemoteShare>, String> {
    let client = smb::Client::new(smb::ClientConfig::default());
    let result = query_with(&client, host, port, username, password).await;
    let _ = client.close().await;
    result
}

async fn query_with(
    client: &smb::Client,
    host: &str,
    port: u16,
    username: &str,
    password: &str,
) -> Result<Vec<RemoteShare>, String> {
    if port != 0 {
        let ip: std::net::IpAddr = host
            .parse()
            .map_err(|_| "a custom port needs an IP address".to_string())?;
        client
            .connect_to_address(host, std::net::SocketAddr::new(ip, port))
            .await
            .map_err(|e| describe(host, e))?;
    }
    client
        .ipc_connect(host, username, password.to_string())
        .await
        .map_err(|e| describe(host, e))?;
    let pipe = client
        .open_pipe(host, "srvsvc")
        .await
        .map_err(|e| describe(host, e))?;

    let bind_ack = transceive(&pipe, rpc::bind_pdu()).await.map_err(|e| describe(host, e))?;
    rpc::check_bind_ack(&bind_ack)?;
    let response = transceive(&pipe, rpc::share_enum_pdu(host))
        .await
        .map_err(|e| describe(host, e))?;

    let mut shares: Vec<RemoteShare> = rpc::parse_share_enum(&response)?
        .into_iter()
        // Disk shares only, and not the hidden administrative ones (C$, ...).
        .filter(|s| s.kind & 0x0FFF_FFFF == 0 && s.kind & 0x8000_0000 == 0)
        .filter(|s| !s.name.is_empty() && !s.name.ends_with('$'))
        .map(|s| RemoteShare {
            name: s.name,
            comment: s.comment,
        })
        .collect();
    shares.sort_by_key(|s| s.name.to_lowercase());
    Ok(shares)
}

async fn transceive(pipe: &smb::Pipe, pdu: Vec<u8>) -> Result<Vec<u8>, smb::Error> {
    let reply = pipe
        .fsctl_with_options(
            smb::PipeTransceiveRequest::from(smb::IoctlBuffer::from(pdu)),
            rpc::MAX_FRAGMENT as u32,
        )
        .await?;
    let bytes: &[u8] = reply.as_ref();
    Ok(bytes.to_vec())
}

fn describe(host: &str, e: smb::Error) -> String {
    let text = e.to_string();
    let lower = text.to_lowercase();
    if lower.contains("logon") || lower.contains("access denied") || lower.contains("password") {
        "The server rejected the username or password.".into()
    } else if lower.contains("invalid address") {
        format!("Could not find a server called {host}.")
    } else if lower.contains("refused")
        || lower.contains("unreachable")
        || lower.contains("timed out")
        || lower.contains("timeout")
        || lower.contains("lookup")
    {
        format!("Could not reach {host}.")
    } else {
        format!("Could not list shares on {host}: {text}")
    }
}

/// Just enough DCE/RPC to bind to srvsvc and call NetrShareEnum at level 1.
mod rpc {
    /// Largest reply fragment we ask for. The whole share list has to fit in
    /// one fragment, which is room for several dozen shares.
    pub const MAX_FRAGMENT: u16 = 5840;

    const PTYPE_REQUEST: u8 = 0;
    const PTYPE_RESPONSE: u8 = 2;
    const PTYPE_FAULT: u8 = 3;
    const PTYPE_BIND: u8 = 11;
    const PTYPE_BIND_ACK: u8 = 12;
    const FIRST_AND_LAST: u8 = 0x03;
    const LAST_FRAGMENT: u8 = 0x02;
    const HEADER_LEN: usize = 16;

    /// srvsvc interface 4b324fc8-1670-01d3-1278-5a47bf6ee188, version 3.0.
    const SRVSVC: [u8; 20] = [
        0xc8, 0x4f, 0x32, 0x4b, 0x70, 0x16, 0xd3, 0x01, 0x12, 0x78, 0x5a, 0x47, 0xbf, 0x6e, 0xe1,
        0x88, 0x03, 0x00, 0x00, 0x00,
    ];
    /// NDR transfer syntax 8a885d04-1ceb-11c9-9fe8-08002b104860, version 2.
    const NDR: [u8; 20] = [
        0x04, 0x5d, 0x88, 0x8a, 0xeb, 0x1c, 0xc9, 0x11, 0x9f, 0xe8, 0x08, 0x00, 0x2b, 0x10, 0x48,
        0x60, 0x02, 0x00, 0x00, 0x00,
    ];
    const OP_NETR_SHARE_ENUM: u16 = 15;

    pub struct ShareEntry {
        pub name: String,
        pub kind: u32,
        pub comment: String,
    }

    fn pdu(ptype: u8, call_id: u32, body: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + body.len());
        out.extend_from_slice(&[5, 0, ptype, FIRST_AND_LAST]);
        out.extend_from_slice(&[0x10, 0, 0, 0]); // little-endian, ASCII, IEEE
        out.extend_from_slice(&((HEADER_LEN + body.len()) as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // no auth trailer
        out.extend_from_slice(&call_id.to_le_bytes());
        out.extend_from_slice(body);
        out
    }

    pub fn bind_pdu() -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&MAX_FRAGMENT.to_le_bytes()); // max transmit
        body.extend_from_slice(&MAX_FRAGMENT.to_le_bytes()); // max receive
        body.extend_from_slice(&0u32.to_le_bytes()); // new association group
        body.extend_from_slice(&[1, 0, 0, 0]); // one presentation context
        body.extend_from_slice(&0u16.to_le_bytes()); // context id 0
        body.extend_from_slice(&[1, 0]); // one transfer syntax
        body.extend_from_slice(&SRVSVC);
        body.extend_from_slice(&NDR);
        pdu(PTYPE_BIND, 1, &body)
    }

    pub fn check_bind_ack(reply: &[u8]) -> Result<(), String> {
        let bad = || "The server sent an unexpected reply to the share-list request.".to_string();
        if reply.len() < HEADER_LEN || reply[2] != PTYPE_BIND_ACK {
            return Err(bad());
        }
        let mut r = Reader::new(&reply[HEADER_LEN..]);
        r.skip(8).ok_or_else(bad)?; // fragment sizes, association group
        let addr_len = r.u16().ok_or_else(bad)? as usize;
        r.skip(addr_len).ok_or_else(bad)?;
        r.align(4);
        let results = r.u8().ok_or_else(bad)?;
        r.skip(3).ok_or_else(bad)?;
        if results < 1 || r.u16().ok_or_else(bad)? != 0 {
            return Err("The server does not offer a share list.".into());
        }
        Ok(())
    }

    pub fn share_enum_pdu(host: &str) -> Vec<u8> {
        let mut stub = Vec::new();
        let u32le = |v: u32, out: &mut Vec<u8>| out.extend_from_slice(&v.to_le_bytes());

        // ServerName: pointer to a null-terminated UTF-16 string.
        let name: Vec<u16> = format!("\\\\{host}").encode_utf16().chain([0]).collect();
        u32le(0x0002_0000, &mut stub);
        u32le(name.len() as u32, &mut stub); // max count
        u32le(0, &mut stub); // offset
        u32le(name.len() as u32, &mut stub); // actual count
        for unit in &name {
            stub.extend_from_slice(&unit.to_le_bytes());
        }
        while stub.len() % 4 != 0 {
            stub.push(0);
        }
        // InfoStruct: level 1, with an empty container for the server to fill.
        u32le(1, &mut stub); // level
        u32le(1, &mut stub); // union discriminant
        u32le(0x0002_0004, &mut stub); // pointer to container
        u32le(0, &mut stub); // entries read
        u32le(0, &mut stub); // null buffer
        u32le(0xFFFF_FFFF, &mut stub); // preferred maximum length: no limit
        u32le(0x0002_0008, &mut stub); // pointer to resume handle
        u32le(0, &mut stub); // resume handle

        let mut body = Vec::new();
        body.extend_from_slice(&(stub.len() as u32).to_le_bytes()); // allocation hint
        body.extend_from_slice(&0u16.to_le_bytes()); // context id
        body.extend_from_slice(&OP_NETR_SHARE_ENUM.to_le_bytes());
        body.extend_from_slice(&stub);
        pdu(PTYPE_REQUEST, 2, &body)
    }

    pub fn parse_share_enum(reply: &[u8]) -> Result<Vec<ShareEntry>, String> {
        let bad = || "The server's share list could not be read.".to_string();
        if reply.len() < HEADER_LEN + 8 {
            return Err(bad());
        }
        match reply[2] {
            PTYPE_RESPONSE => {}
            PTYPE_FAULT => {
                let code = u32::from_le_bytes(reply[24..28].try_into().map_err(|_| bad())?);
                return Err(if code == 5 {
                    "This account is not allowed to list the server's shares.".into()
                } else {
                    format!("The server refused the share-list request (fault {code:#x}).")
                });
            }
            _ => return Err(bad()),
        }
        if reply[3] & LAST_FRAGMENT == 0 {
            return Err("The server has more shares than this version can list.".into());
        }
        let frag_len = u16::from_le_bytes([reply[8], reply[9]]) as usize;
        let stub = reply.get(HEADER_LEN + 8..frag_len.min(reply.len())).ok_or_else(bad)?;
        let mut r = Reader::new(stub);

        r.skip(8).ok_or_else(bad)?; // level, union discriminant
        let mut entries = Vec::new();
        if r.u32().ok_or_else(bad)? != 0 {
            // container
            let count = r.u32().ok_or_else(bad)? as usize;
            if r.u32().ok_or_else(bad)? != 0 {
                // array
                let max = r.u32().ok_or_else(bad)? as usize;
                if count > max || count > stub.len() / 12 {
                    return Err(bad());
                }
                // Fixed parts first, then the strings they point to, in order.
                let mut fixed = Vec::with_capacity(count);
                for _ in 0..count {
                    let name_ptr = r.u32().ok_or_else(bad)?;
                    let kind = r.u32().ok_or_else(bad)?;
                    let comment_ptr = r.u32().ok_or_else(bad)?;
                    fixed.push((name_ptr, kind, comment_ptr));
                }
                for (name_ptr, kind, comment_ptr) in fixed {
                    let name = if name_ptr != 0 {
                        r.string().ok_or_else(bad)?
                    } else {
                        String::new()
                    };
                    let comment = if comment_ptr != 0 {
                        r.string().ok_or_else(bad)?
                    } else {
                        String::new()
                    };
                    entries.push(ShareEntry {
                        name,
                        kind,
                        comment,
                    });
                }
            }
        }
        // Trailer: total entries, resume handle pointer (+ value), status.
        r.skip(4).ok_or_else(bad)?;
        if r.u32().ok_or_else(bad)? != 0 {
            r.skip(4).ok_or_else(bad)?;
        }
        match r.u32().ok_or_else(bad)? {
            0 => Ok(entries),
            5 => Err("This account is not allowed to list the server's shares.".into()),
            code => Err(format!("The server could not list its shares (error {code}).")),
        }
    }

    /// Bounds-checked little-endian cursor; every read can fail, none can panic.
    struct Reader<'a> {
        data: &'a [u8],
        pos: usize,
    }

    impl<'a> Reader<'a> {
        fn new(data: &'a [u8]) -> Self {
            Self { data, pos: 0 }
        }
        fn take(&mut self, n: usize) -> Option<&'a [u8]> {
            let end = self.pos.checked_add(n)?;
            let slice = self.data.get(self.pos..end)?;
            self.pos = end;
            Some(slice)
        }
        fn skip(&mut self, n: usize) -> Option<()> {
            self.take(n).map(|_| ())
        }
        fn align(&mut self, to: usize) {
            self.pos = self.pos.div_ceil(to) * to;
        }
        fn u8(&mut self) -> Option<u8> {
            self.take(1).map(|b| b[0])
        }
        fn u16(&mut self) -> Option<u16> {
            self.take(2).map(|b| u16::from_le_bytes([b[0], b[1]]))
        }
        fn u32(&mut self) -> Option<u32> {
            self.align(4);
            self.take(4)
                .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        }
        /// A conformant varying UTF-16 string: max count, offset, actual
        /// count, then that many code units.
        fn string(&mut self) -> Option<String> {
            self.u32()?;
            self.u32()?;
            let count = self.u32()? as usize;
            let bytes = self.take(count.checked_mul(2)?)?;
            let units: Vec<u16> = bytes
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            Some(
                String::from_utf16_lossy(&units)
                    .trim_end_matches('\0')
                    .to_string(),
            )
        }
    }

    #[cfg(test)]
    pub mod tests {
        use super::*;

        fn put_str(out: &mut Vec<u8>, s: &str) {
            let units: Vec<u16> = s.encode_utf16().chain([0]).collect();
            for v in [units.len() as u32, 0, units.len() as u32] {
                out.extend_from_slice(&v.to_le_bytes());
            }
            for u in &units {
                out.extend_from_slice(&u.to_le_bytes());
            }
            while out.len() % 4 != 0 {
                out.push(0);
            }
        }

        /// Builds a NetrShareEnum response the way a server would.
        pub fn response(shares: &[(&str, u32, &str)], status: u32) -> Vec<u8> {
            let mut stub = Vec::new();
            let put = |v: u32, out: &mut Vec<u8>| out.extend_from_slice(&v.to_le_bytes());
            put(1, &mut stub);
            put(1, &mut stub);
            put(0x20000, &mut stub);
            put(shares.len() as u32, &mut stub);
            put(0x20004, &mut stub);
            put(shares.len() as u32, &mut stub);
            for (i, (_, kind, _)) in shares.iter().enumerate() {
                put(0x20008 + i as u32 * 8, &mut stub);
                put(*kind, &mut stub);
                put(0x2000c + i as u32 * 8, &mut stub);
            }
            for (name, _, comment) in shares {
                put_str(&mut stub, name);
                put_str(&mut stub, comment);
            }
            put(shares.len() as u32, &mut stub);
            put(0, &mut stub); // null resume handle
            put(status, &mut stub);

            let mut body = Vec::new();
            body.extend_from_slice(&(stub.len() as u32).to_le_bytes());
            body.extend_from_slice(&[0, 0, 0, 0]);
            body.extend_from_slice(&stub);
            pdu(PTYPE_RESPONSE, 2, &body)
        }

        #[test]
        fn parses_a_share_list() {
            let reply = response(
                &[
                    ("Media", 0, "Films and music"),
                    ("IPC$", 0x8000_0003, "IPC Service"),
                    ("odd", 0, ""),
                ],
                0,
            );
            let shares = parse_share_enum(&reply).unwrap();
            assert_eq!(shares.len(), 3);
            assert_eq!(shares[0].name, "Media");
            assert_eq!(shares[0].comment, "Films and music");
            assert_eq!(shares[1].kind, 0x8000_0003);
            assert_eq!(shares[2].name, "odd");
            assert_eq!(shares[2].comment, "");
        }

        #[test]
        fn empty_list_and_errors() {
            assert!(parse_share_enum(&response(&[], 0)).unwrap().is_empty());
            assert!(parse_share_enum(&response(&[], 5)).is_err());
        }

        #[test]
        fn truncated_replies_fail_cleanly() {
            let reply = response(&[("Media", 0, "x")], 0);
            for len in 0..reply.len() {
                let mut cut = reply[..len].to_vec();
                if cut.len() >= 10 {
                    let l = cut.len() as u16;
                    cut[8..10].copy_from_slice(&l.to_le_bytes());
                }
                assert!(parse_share_enum(&cut).is_err(), "length {len}");
            }
        }

        #[test]
        fn request_is_well_formed() {
            let req = share_enum_pdu("192.168.1.10");
            assert_eq!(req[2], PTYPE_REQUEST);
            assert_eq!(u16::from_le_bytes([req[8], req[9]]) as usize, req.len());
            assert_eq!((req.len() - 24) % 4, 0);
            let bind = bind_pdu();
            assert_eq!(bind.len(), 72);
        }
    }
}

#[cfg(test)]
mod tests {
    /// Talks to a real server. Run with, for example:
    /// `HEXY_TEST_HOST=192.168.1.10 HEXY_TEST_USER=me HEXY_TEST_PASS=... cargo test -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn lists_shares_on_a_real_server() {
        let host = std::env::var("HEXY_TEST_HOST").expect("HEXY_TEST_HOST");
        let user = std::env::var("HEXY_TEST_USER").expect("HEXY_TEST_USER");
        let pass = std::env::var("HEXY_TEST_PASS").expect("HEXY_TEST_PASS");
        let port = std::env::var("HEXY_TEST_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(0);
        let shares = super::list_shares_on_port(&host, port, &user, &pass)
            .await
            .expect("share listing failed");
        for s in &shares {
            println!("{:<24} {}", s.name, s.comment);
        }
        if let Ok(expected) = std::env::var("HEXY_TEST_EXPECT") {
            let names: Vec<&str> = shares.iter().map(|s| s.name.as_str()).collect();
            assert_eq!(names.join(","), expected);
        }
    }
}

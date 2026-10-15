//! Minimal DNS client over UDP (RFC 1035): resolver benchmarks and lookups for the DNS page.
//! No dependencies; queries go straight to the chosen resolver, bypassing the system cache.

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket},
    time::{Duration, Instant},
};

use serde::Serialize;

pub const TYPE_A: u16 = 1;
pub const TYPE_CNAME: u16 = 5;
pub const TYPE_AAAA: u16 = 28;

/// Domains for timing: popular enough to be cached by every resolver, so we measure the
/// resolver's response time rather than the authoritative servers'.
const BENCH_NAMES: &[&str] = &["google.com", "youtube.com", "apple.com", "github.com", "wikipedia.org", "cloudflare.com", "amazon.com", "x.com"];

pub fn build_query(id: u16, name: &str, qtype: u16) -> Vec<u8> {
    let mut q = Vec::with_capacity(32 + name.len());
    q.extend_from_slice(&id.to_be_bytes());
    q.extend_from_slice(&[0x01, 0x00]); // standard query, recursion desired
    q.extend_from_slice(&[0, 1, 0, 0, 0, 0, 0, 0]); // 1 question
    for label in name.trim_end_matches('.').split('.').filter(|l| !l.is_empty()) {
        let l = &label.as_bytes()[..label.len().min(63)];
        q.push(l.len() as u8);
        q.extend_from_slice(l);
    }
    q.push(0);
    q.extend_from_slice(&qtype.to_be_bytes());
    q.extend_from_slice(&1u16.to_be_bytes()); // class IN
    q
}

/// Reads a (possibly compressed) name at `pos`; returns it and the position after it.
fn read_name(msg: &[u8], mut pos: usize) -> Option<(String, usize)> {
    let mut labels = Vec::new();
    let mut end = None;
    for _ in 0..64 {
        let len = *msg.get(pos)? as usize;
        if len == 0 {
            return Some((labels.join("."), end.unwrap_or(pos + 1)));
        }
        if len & 0xC0 == 0xC0 {
            let ptr = ((len & 0x3F) << 8) | *msg.get(pos + 1)? as usize;
            end.get_or_insert(pos + 2);
            pos = ptr;
            continue;
        }
        labels.push(String::from_utf8_lossy(msg.get(pos + 1..pos + 1 + len)?).into_owned());
        pos += 1 + len;
    }
    None
}

#[derive(Serialize, Debug, PartialEq)]
pub struct Record {
    pub kind: &'static str,
    pub value: String,
    pub ttl: u32,
}

/// Parses answers out of a response; `Err` carries the DNS error name.
pub fn parse_answers(msg: &[u8]) -> Result<Vec<Record>, String> {
    if msg.len() < 12 {
        return Err("short response".into());
    }
    let rcode = msg[3] & 0x0F;
    if rcode != 0 {
        return Err(match rcode {
            2 => "SERVFAIL".into(),
            3 => "NXDOMAIN (no such domain)".into(),
            5 => "REFUSED".into(),
            n => format!("error code {n}"),
        });
    }
    let qd = u16::from_be_bytes([msg[4], msg[5]]);
    let an = u16::from_be_bytes([msg[6], msg[7]]);
    let mut pos = 12;
    for _ in 0..qd {
        pos = read_name(msg, pos).ok_or("bad question")?.1 + 4;
    }
    let mut out = Vec::new();
    for _ in 0..an {
        let (_, p) = read_name(msg, pos).ok_or("bad answer")?;
        let h = msg.get(p..p + 10).ok_or("truncated answer")?;
        let (rtype, ttl, len) = (u16::from_be_bytes([h[0], h[1]]), u32::from_be_bytes([h[4], h[5], h[6], h[7]]), u16::from_be_bytes([h[8], h[9]]) as usize);
        let data = msg.get(p + 10..p + 10 + len).ok_or("truncated data")?;
        match rtype {
            TYPE_A if len == 4 => out.push(Record { kind: "A", value: Ipv4Addr::new(data[0], data[1], data[2], data[3]).to_string(), ttl }),
            TYPE_AAAA if len == 16 => {
                let b: [u8; 16] = data.try_into().map_err(|_| "bad AAAA")?;
                out.push(Record { kind: "AAAA", value: Ipv6Addr::from(b).to_string(), ttl });
            }
            TYPE_CNAME => out.push(Record { kind: "CNAME", value: read_name(msg, p + 10).ok_or("bad CNAME")?.0, ttl }),
            _ => {}
        }
        pos = p + 10 + len;
    }
    Ok(out)
}

fn server_addr(server: &str) -> Option<SocketAddr> {
    server.parse::<IpAddr>().ok().map(|ip| SocketAddr::new(ip, 53))
}

/// One query; returns round-trip time and the raw response.
pub fn query(server: &str, name: &str, qtype: u16, timeout: Duration) -> Result<(Duration, Vec<u8>), String> {
    let addr = server_addr(server).ok_or("not an IP address")?;
    let bind: SocketAddr = if addr.is_ipv6() { "[::]:0".parse().unwrap() } else { "0.0.0.0:0".parse().unwrap() };
    let sock = UdpSocket::bind(bind).map_err(|e| e.to_string())?;
    sock.set_read_timeout(Some(timeout)).map_err(|e| e.to_string())?;
    let id = u16::from_le_bytes(crate::profiles::random_bytes::<2>());
    let packet = build_query(id, name, qtype);
    let start = Instant::now();
    sock.send_to(&packet, addr).map_err(|e| e.to_string())?;
    let mut buf = [0u8; 1500];
    loop {
        let (n, from) = sock.recv_from(&mut buf).map_err(|_| "timed out".to_string())?;
        // Ignore anything that isn't the answer to our question from that server.
        if from.ip() == addr.ip() && n >= 2 && buf[..2] == id.to_be_bytes() {
            return Ok((start.elapsed(), buf[..n].to_vec()));
        }
        if start.elapsed() > timeout {
            return Err("timed out".into());
        }
    }
}

#[derive(Serialize)]
pub struct Bench {
    pub server: String,
    /// Median round trip, ms.
    pub median: Option<f64>,
    pub best: Option<f64>,
    pub ok: usize,
    pub total: usize,
}

/// Times a resolver across several cached names (first query warms the path).
pub fn bench(server: &str) -> Bench {
    let timeout = Duration::from_millis(1200);
    let _ = query(server, BENCH_NAMES[0], TYPE_A, timeout);
    let mut times: Vec<f64> = BENCH_NAMES
        .iter()
        .filter_map(|n| query(server, n, TYPE_A, timeout).ok().map(|(d, _)| d.as_secs_f64() * 1000.0))
        .collect();
    times.sort_by(f64::total_cmp);
    Bench {
        server: server.to_string(),
        median: times.get(times.len() / 2).copied(),
        best: times.first().copied(),
        ok: times.len(),
        total: BENCH_NAMES.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_encodes_labels() {
        let q = build_query(0xABCD, "www.example.com", TYPE_A);
        assert_eq!(&q[..2], &[0xAB, 0xCD]);
        assert_eq!(&q[12..], b"\x03www\x07example\x03com\x00\x00\x01\x00\x01");
    }

    #[test]
    fn parses_compressed_answers() {
        // Response for example.com: CNAME (pointer back to the question name) then an A record.
        let mut r = build_query(1, "example.com", TYPE_A);
        r[2] = 0x81;
        r[3] = 0x80;
        r[7] = 2; // two answers
        r.extend_from_slice(&[0xC0, 12, 0, 5, 0, 1, 0, 0, 0, 60, 0, 2, 0xC0, 12]);
        r.extend_from_slice(&[0xC0, 12, 0, 1, 0, 1, 0, 0, 1, 0, 0, 4, 93, 184, 216, 34]);
        let a = parse_answers(&r).unwrap();
        assert_eq!(a[0], Record { kind: "CNAME", value: "example.com".into(), ttl: 60 });
        assert_eq!(a[1], Record { kind: "A", value: "93.184.216.34".into(), ttl: 256 });
    }

    #[test]
    fn reports_nxdomain() {
        let mut r = build_query(1, "nope.invalid", TYPE_A);
        r[3] = 0x83;
        assert!(parse_answers(&r).unwrap_err().starts_with("NXDOMAIN"));
    }
}

//! Just enough of the DNS wire format (RFC 1035) to ask name servers for TXT, NS, CNAME
//! and address records: queries over UDP with a TCP retry when the answer is truncated,
//! and responses with name compression.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpStream, UdpSocket};

pub(crate) const TYPE_A: u16 = 1;
pub(crate) const TYPE_NS: u16 = 2;
pub(crate) const TYPE_CNAME: u16 = 5;
pub(crate) const TYPE_TXT: u16 = 16;
pub(crate) const TYPE_AAAA: u16 = 28;
const CLASS_IN: u16 = 1;

pub(crate) const RCODE_NXDOMAIN: u8 = 3;

/// One resource record, with the data this module understands decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Record {
    pub name: String,
    pub data: RData,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RData {
    A(Ipv4Addr),
    Aaaa(Ipv6Addr),
    Ns(String),
    Cname(String),
    /// A TXT record's character-strings, joined (RFC 8555 values are one string).
    Txt(String),
    Other,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Message {
    pub id: u16,
    pub authoritative: bool,
    pub truncated: bool,
    pub rcode: u8,
    pub answers: Vec<Record>,
    pub authority: Vec<Record>,
    pub additional: Vec<Record>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum WireError {
    #[error("malformed DNS message")]
    Malformed,
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("no answer within {0:?}")]
    Timeout(Duration),
}

/// Lowercase, no trailing dot.
pub(crate) fn canonical(name: &str) -> String {
    name.trim_end_matches('.').to_ascii_lowercase()
}

pub(crate) fn encode_query(id: u16, name: &str, qtype: u16, recursion: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&(if recursion { 0x0100u16 } else { 0 }).to_be_bytes());
    out.extend_from_slice(&1u16.to_be_bytes());
    out.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
    for label in canonical(name).split('.').filter(|l| !l.is_empty()) {
        let bytes = label.as_bytes();
        out.push(u8::try_from(bytes.len().min(63)).unwrap_or(63));
        out.extend_from_slice(&bytes[..bytes.len().min(63)]);
    }
    out.push(0);
    out.extend_from_slice(&qtype.to_be_bytes());
    out.extend_from_slice(&CLASS_IN.to_be_bytes());
    out
}

struct Reader<'a> {
    buf: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn u8(&mut self) -> Result<u8, WireError> {
        let v = *self.buf.get(self.at).ok_or(WireError::Malformed)?;
        self.at += 1;
        Ok(v)
    }

    fn u16(&mut self) -> Result<u16, WireError> {
        Ok(u16::from_be_bytes([self.u8()?, self.u8()?]))
    }

    fn u32(&mut self) -> Result<u32, WireError> {
        Ok(u32::from_be_bytes([
            self.u8()?,
            self.u8()?,
            self.u8()?,
            self.u8()?,
        ]))
    }

    fn bytes(&mut self, n: usize) -> Result<&[u8], WireError> {
        let end = self.at.checked_add(n).ok_or(WireError::Malformed)?;
        let s = self.buf.get(self.at..end).ok_or(WireError::Malformed)?;
        self.at = end;
        Ok(s)
    }

    /// A possibly compressed name, starting at the cursor.
    fn name(&mut self) -> Result<String, WireError> {
        let mut labels: Vec<String> = Vec::new();
        let mut at = self.at;
        let mut jumped = false;
        let mut hops = 0;
        loop {
            let len = *self.buf.get(at).ok_or(WireError::Malformed)?;
            match len {
                0 => {
                    at += 1;
                    break;
                }
                l if l & 0xc0 == 0xc0 => {
                    let lo = *self.buf.get(at + 1).ok_or(WireError::Malformed)?;
                    if !jumped {
                        self.at = at + 2;
                    }
                    jumped = true;
                    hops += 1;
                    if hops > 32 {
                        return Err(WireError::Malformed);
                    }
                    at = usize::from(u16::from_be_bytes([l & 0x3f, lo]));
                }
                l if l & 0xc0 == 0 => {
                    let l = usize::from(l);
                    let label = self
                        .buf
                        .get(at + 1..at + 1 + l)
                        .ok_or(WireError::Malformed)?;
                    labels.push(String::from_utf8_lossy(label).to_ascii_lowercase());
                    at += 1 + l;
                }
                _ => return Err(WireError::Malformed),
            }
        }
        if !jumped {
            self.at = at;
        }
        Ok(labels.join("."))
    }

    fn record(&mut self) -> Result<Record, WireError> {
        let name = self.name()?;
        let rtype = self.u16()?;
        let _class = self.u16()?;
        let _ttl = self.u32()?;
        let len = usize::from(self.u16()?);
        let start = self.at;
        let data = match rtype {
            TYPE_A if len == 4 => {
                let b = self.bytes(4)?;
                RData::A(Ipv4Addr::new(b[0], b[1], b[2], b[3]))
            }
            TYPE_AAAA if len == 16 => {
                let b: [u8; 16] = self
                    .bytes(16)?
                    .try_into()
                    .map_err(|_| WireError::Malformed)?;
                RData::Aaaa(Ipv6Addr::from(b))
            }
            TYPE_NS => RData::Ns(self.name()?),
            TYPE_CNAME => RData::Cname(self.name()?),
            TYPE_TXT => {
                let mut text = Vec::new();
                while self.at < start + len {
                    let n = usize::from(self.u8()?);
                    text.extend_from_slice(self.bytes(n)?);
                }
                RData::Txt(String::from_utf8_lossy(&text).into_owned())
            }
            _ => RData::Other,
        };
        // Whatever the data held, the next record starts after it.
        self.at = start.checked_add(len).ok_or(WireError::Malformed)?;
        if self.at > self.buf.len() {
            return Err(WireError::Malformed);
        }
        Ok(Record { name, data })
    }
}

pub(crate) fn decode(buf: &[u8]) -> Result<Message, WireError> {
    let mut r = Reader { buf, at: 0 };
    let id = r.u16()?;
    let flags = r.u16()?;
    let counts = [r.u16()?, r.u16()?, r.u16()?, r.u16()?];
    for _ in 0..counts[0] {
        r.name()?;
        r.u16()?;
        r.u16()?;
    }
    let mut sections = [Vec::new(), Vec::new(), Vec::new()];
    for (section, &count) in sections.iter_mut().zip(&counts[1..]) {
        for _ in 0..count {
            section.push(r.record()?);
        }
    }
    let [answers, authority, additional] = sections;
    Ok(Message {
        id,
        authoritative: flags & 0x0400 != 0,
        truncated: flags & 0x0200 != 0,
        rcode: u8::try_from(flags & 0x000f).unwrap_or(0),
        answers,
        authority,
        additional,
    })
}

/// Ask `server` one question: UDP first, TCP when the answer comes back truncated.
pub(crate) async fn query(
    server: SocketAddr,
    name: &str,
    qtype: u16,
    recursion: bool,
    timeout: Duration,
) -> Result<Message, WireError> {
    let id: u16 = rand::random();
    let q = encode_query(id, name, qtype, recursion);
    let udp = tokio::time::timeout(timeout, async {
        let bind: SocketAddr = match server.ip() {
            IpAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
            IpAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
        };
        // Loopback servers (tests, local resolvers) get a loopback socket: nothing listens
        // on the LAN for it.
        let bind = if server.ip().is_loopback() {
            SocketAddr::new(server.ip(), 0)
        } else {
            bind
        };
        let sock = UdpSocket::bind(bind).await?;
        sock.connect(server).await?;
        sock.send(&q).await?;
        let mut buf = vec![0u8; 4096];
        loop {
            let n = sock.recv(&mut buf).await?;
            if let Ok(m) = decode(&buf[..n])
                && m.id == id
            {
                return Ok::<_, WireError>(m);
            }
        }
    })
    .await
    .map_err(|_| WireError::Timeout(timeout))??;
    if !udp.truncated {
        return Ok(udp);
    }
    tokio::time::timeout(timeout, async {
        let mut s = TcpStream::connect(server).await?;
        let len = u16::try_from(q.len()).map_err(|_| WireError::Malformed)?;
        s.write_all(&len.to_be_bytes()).await?;
        s.write_all(&q).await?;
        let n = usize::from(s.read_u16().await?);
        let mut buf = vec![0u8; n];
        s.read_exact(&mut buf).await?;
        decode(&buf)
    })
    .await
    .map_err(|_| WireError::Timeout(timeout))?
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Build a response to `query` (as a server would), for fake servers in tests.
    pub(crate) fn respond(
        query: &[u8],
        authoritative: bool,
        rcode: u8,
        sections: [&[(String, u16, Vec<u8>)]; 3],
    ) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&query[..2]);
        let mut flags = 0x8000u16 | u16::from(rcode);
        if authoritative {
            flags |= 0x0400;
        }
        out.extend_from_slice(&flags.to_be_bytes());
        out.extend_from_slice(&1u16.to_be_bytes());
        for s in &sections {
            out.extend_from_slice(&u16::try_from(s.len()).unwrap().to_be_bytes());
        }
        // The question, copied.
        let mut r = Reader { buf: query, at: 12 };
        r.name().unwrap();
        let qend = r.at + 4;
        out.extend_from_slice(&query[12..qend]);
        for s in sections {
            for (name, rtype, data) in s {
                out.extend_from_slice(&name_bytes(name));
                out.extend_from_slice(&rtype.to_be_bytes());
                out.extend_from_slice(&CLASS_IN.to_be_bytes());
                out.extend_from_slice(&60u32.to_be_bytes());
                out.extend_from_slice(&u16::try_from(data.len()).unwrap().to_be_bytes());
                out.extend_from_slice(data);
            }
        }
        out
    }

    pub(crate) fn name_bytes(name: &str) -> Vec<u8> {
        let mut out = Vec::new();
        for l in canonical(name).split('.').filter(|l| !l.is_empty()) {
            out.push(u8::try_from(l.len()).unwrap());
            out.extend_from_slice(l.as_bytes());
        }
        out.push(0);
        out
    }

    pub(crate) fn txt(value: &str) -> Vec<u8> {
        let mut v = vec![u8::try_from(value.len()).unwrap()];
        v.extend_from_slice(value.as_bytes());
        v
    }

    /// The name and type a query asks about.
    pub(crate) fn question(query: &[u8]) -> (String, u16) {
        let mut r = Reader { buf: query, at: 12 };
        let name = r.name().unwrap();
        (name, r.u16().unwrap())
    }

    #[test]
    fn queries_and_compressed_answers() {
        let q = encode_query(0x1234, "_acme-challenge.Example.COM.", TYPE_TXT, false);
        assert_eq!(&q[..4], &[0x12, 0x34, 0, 0], "no recursion asked");
        assert_eq!(
            question(&q),
            ("_acme-challenge.example.com".into(), TYPE_TXT)
        );

        // A hand-made response that compresses names against the question.
        let mut m = q.clone();
        m[2] = 0x84; // QR, AA
        m[7] = 2; // two answers
        // TXT, name pointer to offset 12, two character-strings.
        m.extend_from_slice(&[
            0xc0, 12, 0, 16, 0, 1, 0, 0, 0, 60, 0, 6, 2, b'a', b'b', 2, b'c', b'd',
        ]);
        // CNAME pointing into the question's "example.com".
        m.extend_from_slice(&[0xc0, 12, 0, 5, 0, 1, 0, 0, 0, 60, 0, 2, 0xc0, 28]);
        let d = decode(&m).unwrap();
        assert!(d.authoritative && !d.truncated);
        assert_eq!(d.answers[0].data, RData::Txt("abcd".into()));
        assert_eq!(d.answers[1].data, RData::Cname("example.com".into()));
        assert_eq!(d.answers[1].name, "_acme-challenge.example.com");

        // Loops and truncation are errors, not hangs.
        let mut looped = q.clone();
        looped[7] = 1;
        let at = u8::try_from(looped.len()).unwrap();
        looped.extend_from_slice(&[0xc0, at]);
        assert!(decode(&looped).is_err());
        assert!(decode(&q[..10]).is_err());
    }
}

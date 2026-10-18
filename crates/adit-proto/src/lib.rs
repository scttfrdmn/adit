// SPDX-License-Identifier: Apache-2.0
//! The adit wire protocol, shared by proxy and client.
//!
//! **Control plane.** The client registers once per proxy with `PUT /_adit/agents/{name}`, body =
//! its serialized NIXL agent metadata. Each object (or stripe of one) is then one
//! `GET /{bucket}/{key}` carrying the headers below. The proxy answers with no body once every
//! chunk it owns has been written (`200`) or has failed (`502`), reporting what it wrote.
//!
//! **Data plane.** Each chunk is one NIXL WRITE into the client's buffer carrying a notification
//! [`Notif`]; the client counts coverage from notifications, independently of the HTTP answer.
//! A request is complete only when both agree (see `adit-client`).

use std::fmt;
use std::ops::Range;
use std::str::FromStr;

/// Path prefix for agent registration: `PUT {AGENTS_PATH}/{name}`.
pub const AGENTS_PATH: &str = "/_adit/agents";

/// Presigned GET URL for the object. The proxy reads nothing the client could not.
pub const H_PRESIGNED_URL: &str = "x-adit-presigned-url";
/// Name of the client's NIXL agent, as registered.
pub const H_AGENT: &str = "x-adit-agent";
/// The client's registered destination region: [`Dest`].
pub const H_DEST: &str = "x-adit-dest";
/// Offset into the destination region where the first byte of `H_RANGE` goes.
pub const H_DEST_OFFSET: &str = "x-adit-dest-offset";
/// Object byte range this proxy owns, HTTP style inclusive: `bytes=a-b`.
pub const H_RANGE: &str = "x-adit-range";
/// Client-chosen request id, echoed in every notification.
pub const H_REQUEST_ID: &str = "x-adit-request-id";
/// Optional per-request chunk size override, bytes.
pub const H_CHUNK: &str = "x-adit-chunk";

/// Response: payload bytes written into the destination.
pub const H_BYTES_WRITTEN: &str = "x-adit-bytes-written";
/// Response: S3 requests issued, including retries.
pub const H_S3_REQUESTS: &str = "x-adit-s3-requests";
/// Response on failure: object ranges not written, `a-b,c-d` (exclusive ends).
pub const H_FAILED_RANGES: &str = "x-adit-failed-ranges";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("malformed {what}: {value:?}")]
pub struct ParseError {
    pub what: &'static str,
    pub value: String,
}

fn bad(what: &'static str, value: &str) -> ParseError {
    ParseError { what, value: value.to_string() }
}

/// A registered remote memory region: base address, length, device id (CUDA ordinal for VRAM).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dest {
    pub addr: u64,
    pub len: u64,
    pub dev: u64,
}

impl fmt::Display for Dest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#x},{},{}", self.addr, self.len, self.dev)
    }
}

impl FromStr for Dest {
    type Err = ParseError;
    fn from_str(s: &str) -> Result<Self, ParseError> {
        let mut it = s.split(',');
        let (Some(a), Some(l), Some(d), None) = (it.next(), it.next(), it.next(), it.next()) else {
            return Err(bad("dest", s));
        };
        let addr = a.strip_prefix("0x").map_or_else(|| a.parse().ok(), |h| u64::from_str_radix(h, 16).ok());
        match (addr, l.parse(), d.parse()) {
            (Some(addr), Ok(len), Ok(dev)) => Ok(Dest { addr, len, dev }),
            _ => Err(bad("dest", s)),
        }
    }
}

/// Format an exclusive range as an HTTP inclusive `bytes=a-b`.
pub fn format_range(r: &Range<u64>) -> String {
    format!("bytes={}-{}", r.start, r.end - 1)
}

/// Parse an HTTP inclusive `bytes=a-b` into an exclusive range.
pub fn parse_range(s: &str) -> Result<Range<u64>, ParseError> {
    let v = s.strip_prefix("bytes=").ok_or_else(|| bad("range", s))?;
    let (a, b) = v.split_once('-').ok_or_else(|| bad("range", s))?;
    match (a.parse::<u64>(), b.parse::<u64>()) {
        (Ok(a), Ok(b)) if a <= b && b < u64::MAX => Ok(a..b + 1),
        _ => Err(bad("range", s)),
    }
}

/// Format ranges as `a-b,c-d` with exclusive ends.
pub fn format_ranges(rs: &[Range<u64>]) -> String {
    rs.iter().map(|r| format!("{}-{}", r.start, r.end)).collect::<Vec<_>>().join(",")
}

pub fn parse_ranges(s: &str) -> Result<Vec<Range<u64>>, ParseError> {
    if s.is_empty() {
        return Ok(vec![]);
    }
    s.split(',')
        .map(|p| {
            let (a, b) = p.split_once('-').ok_or_else(|| bad("ranges", s))?;
            match (a.parse::<u64>(), b.parse::<u64>()) {
                (Ok(a), Ok(b)) if a < b => Ok(a..b),
                _ => Err(bad("ranges", s)),
            }
        })
        .collect()
}

/// One chunk's completion notification: `len` bytes written at destination offset `offset` for
/// request `req`. Delivered by NIXL after the write it accompanies is visible at the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Notif {
    pub req: u64,
    pub offset: u64,
    pub len: u64,
}

impl Notif {
    pub fn encode(&self) -> Vec<u8> {
        format!("adit1 {} {} {}", self.req, self.offset, self.len).into_bytes()
    }

    pub fn decode(b: &[u8]) -> Result<Self, ParseError> {
        let s = std::str::from_utf8(b).map_err(|_| bad("notif", &String::from_utf8_lossy(b)))?;
        let mut it = s.split(' ');
        match (it.next(), it.next(), it.next(), it.next(), it.next()) {
            (Some("adit1"), Some(r), Some(o), Some(l), None) => match (r.parse(), o.parse(), l.parse()) {
                (Ok(req), Ok(offset), Ok(len)) => Ok(Notif { req, offset, len }),
                _ => Err(bad("notif", s)),
            },
            _ => Err(bad("notif", s)),
        }
    }
}

/// A set of disjoint byte ranges: what has landed for one request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Coverage {
    // Sorted, disjoint, non-adjacent.
    ranges: Vec<Range<u64>>,
}

impl Coverage {
    /// Add `r`; returns the number of bytes in `r` that were already covered (overlap means a
    /// duplicate or overlapping write, which the caller treats as a defect).
    pub fn insert(&mut self, r: Range<u64>) -> u64 {
        if r.is_empty() {
            return 0;
        }
        let mut overlap = 0;
        let mut merged = r.clone();
        let mut out = Vec::with_capacity(self.ranges.len() + 1);
        for x in self.ranges.drain(..) {
            if x.end < merged.start || x.start > merged.end {
                out.push(x);
            } else {
                overlap += x.end.min(r.end).saturating_sub(x.start.max(r.start));
                merged = merged.start.min(x.start)..merged.end.max(x.end);
            }
        }
        out.push(merged);
        out.sort_by_key(|x| x.start);
        self.ranges = out;
        overlap
    }

    pub fn covered(&self) -> u64 {
        self.ranges.iter().map(|r| r.end - r.start).sum()
    }

    pub fn ranges(&self) -> &[Range<u64>] {
        &self.ranges
    }

    /// The parts of `within` not covered.
    pub fn missing(&self, within: Range<u64>) -> Vec<Range<u64>> {
        let mut out = Vec::new();
        let mut at = within.start;
        for r in &self.ranges {
            if r.end <= at || r.start >= within.end {
                continue;
            }
            if r.start > at {
                out.push(at..r.start);
            }
            at = at.max(r.end);
        }
        if at < within.end {
            out.push(at..within.end);
        }
        out
    }

    pub fn is_complete(&self, within: Range<u64>) -> bool {
        self.missing(within).is_empty()
    }
}

#[cfg(test)]
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::*;

    #[test]
    fn dest_round_trips() {
        let d = Dest { addr: 0x7f00_dead_0000, len: 1 << 30, dev: 3 };
        assert_eq!(d.to_string().parse::<Dest>().unwrap(), d);
        assert!("1,2".parse::<Dest>().is_err());
        assert!("0xzz,2,0".parse::<Dest>().is_err());
    }

    #[test]
    fn ranges_round_trip() {
        assert_eq!(parse_range(&format_range(&(5..10))).unwrap(), 5..10);
        assert!(parse_range("bytes=10-5").is_err());
        assert!(parse_range("5-10").is_err());
        let rs = vec![0..4, 9..12];
        assert_eq!(parse_ranges(&format_ranges(&rs)).unwrap(), rs);
        assert_eq!(parse_ranges("").unwrap(), vec![]);
    }

    #[test]
    fn notif_round_trips_and_rejects_junk() {
        let n = Notif { req: 42, offset: 8 << 20, len: 8 << 20 };
        assert_eq!(Notif::decode(&n.encode()).unwrap(), n);
        assert!(Notif::decode(b"adit2 1 2 3").is_err());
        assert!(Notif::decode(b"adit1 1 2").is_err());
        assert!(Notif::decode(b"adit1 1 2 3 4").is_err());
    }

    #[test]
    fn coverage_out_of_order_completes() {
        let mut c = Coverage::default();
        for r in [16..24, 0..8, 24..30, 8..16] {
            assert_eq!(c.insert(r), 0);
        }
        assert_eq!(c.ranges(), &[0..30]);
        assert!(c.is_complete(0..30));
        assert_eq!(c.covered(), 30);
    }

    #[test]
    fn coverage_reports_missing_and_overlap() {
        let mut c = Coverage::default();
        c.insert(0..8);
        c.insert(16..24);
        assert_eq!(c.missing(0..30), vec![8..16, 24..30]);
        assert_eq!(c.insert(4..12), 4);
        assert_eq!(c.missing(0..30), vec![12..16, 24..30]);
    }
}

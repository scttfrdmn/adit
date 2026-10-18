// SPDX-License-Identifier: Apache-2.0
//! The oracle (law 2): per-part SHA-256 digests of an object, their composite, and the manifest
//! that records them.
//!
//! Bench objects are uploaded as multipart with SHA-256 checksums at a fixed part size, so S3
//! stores `ChecksumSHA256 = base64(sha256(d_1 || ... || d_n)) + "-n"` — the composite of the part
//! digests. Computing the same composite from the GPU buffer checks the bytes in HBM against S3
//! itself; the manifest (written at generation time, plain whole-object SHA-256 included) is the
//! second, independent reference. A per-part mismatch names the bad byte range.

use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub type Digest32 = [u8; 32];

/// SHA-256 of every `part`-byte part of `data` (last part short).
pub fn sha256_parts_cpu(data: &[u8], part: usize) -> Vec<Digest32> {
    data.chunks(part).map(|c| Sha256::digest(c).into()).collect()
}

/// S3's composite checksum string for these part digests: `base64(sha256(concat)) + "-n"`.
pub fn composite(parts: &[Digest32]) -> String {
    let mut h = Sha256::new();
    for d in parts {
        h.update(d);
    }
    format!("{}-{}", base64::engine::general_purpose::STANDARD.encode(h.finalize()), parts.len())
}

/// What generation recorded for one object (`bench/workloads/<id>/manifest.jsonl`, one per line).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObjectManifest {
    pub bucket: String,
    pub key: String,
    pub size: u64,
    /// Whole-object SHA-256, hex.
    pub sha256: String,
    pub part_size: u64,
    /// Per-part SHA-256, hex.
    pub parts: Vec<String>,
    /// S3-style composite: `base64(sha256(concat(parts))) + "-n"`.
    pub composite: String,
    /// Generator seed, so the object can be regenerated and re-checked.
    #[serde(default)]
    pub seed: Option<u64>,
}

/// The result of checking one buffer against its manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub composite: String,
    pub matches: bool,
    /// Byte ranges of mismatching parts.
    pub bad_parts: Vec<std::ops::Range<u64>>,
}

impl ObjectManifest {
    pub fn check(&self, parts: &[Digest32]) -> Verdict {
        let mut bad = Vec::new();
        for (i, d) in parts.iter().enumerate() {
            if self.parts.get(i).map(String::as_str) != Some(hex::encode(d).as_str()) {
                let s = i as u64 * self.part_size;
                bad.push(s..(s + self.part_size).min(self.size));
            }
        }
        if parts.len() != self.parts.len() {
            bad.push(0..self.size);
        }
        let composite = composite(parts);
        let matches = bad.is_empty() && composite == self.composite;
        Verdict { composite, matches, bad_parts: bad }
    }
}

#[cfg(test)]
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::*;

    #[test]
    fn composite_matches_s3_format() {
        // Two parts "a" and "b": S3 composite is base64(sha256(sha256("a") || sha256("b"))) + "-2".
        let parts = sha256_parts_cpu(b"ab", 1);
        let mut h = Sha256::new();
        h.update(Sha256::digest(b"a"));
        h.update(Sha256::digest(b"b"));
        let want = format!("{}-2", base64::engine::general_purpose::STANDARD.encode(h.finalize()));
        assert_eq!(composite(&parts), want);
    }

    #[test]
    fn check_names_the_bad_part() {
        let data: Vec<u8> = (0..1000u32).map(|i| i as u8).collect();
        let parts = sha256_parts_cpu(&data, 256);
        let m = ObjectManifest {
            bucket: "b".into(),
            key: "k".into(),
            size: 1000,
            sha256: hex::encode(Sha256::digest(&data)),
            part_size: 256,
            parts: parts.iter().map(hex::encode).collect(),
            composite: composite(&parts),
            seed: None,
        };
        assert!(m.check(&parts).matches);
        let mut bad = data.clone();
        bad[700] ^= 1;
        let v = m.check(&sha256_parts_cpu(&bad, 256));
        assert!(!v.matches);
        assert_eq!(v.bad_parts, vec![512..768]);
    }
}

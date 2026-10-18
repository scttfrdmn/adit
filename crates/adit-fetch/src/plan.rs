// SPDX-License-Identifier: Apache-2.0
//! Splitting object ranges into chunks (one ranged GET each) and stripes (one proxy each).

use std::ops::Range;

/// One ranged GET: `len` bytes at object offset `offset`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chunk {
    pub index: usize,
    pub offset: u64,
    pub len: u64,
}

impl Chunk {
    pub fn range(&self) -> Range<u64> {
        self.offset..self.offset + self.len
    }
}

/// Split `range` into chunks of at most `chunk_size` bytes. Chunk boundaries are aligned to
/// `chunk_size` in object coordinates, so the same object always splits the same way regardless
/// of where a request starts; the first and last chunk may be short.
pub fn chunks(range: Range<u64>, chunk_size: u64) -> Vec<Chunk> {
    assert!(chunk_size > 0, "chunk_size must be positive");
    let mut out = Vec::new();
    let mut at = range.start;
    while at < range.end {
        let boundary = (at / chunk_size + 1) * chunk_size;
        let end = boundary.min(range.end);
        out.push(Chunk { index: out.len(), offset: at, len: end - at });
        at = end;
    }
    out
}

/// Split `range` into at most `n` contiguous stripes whose interior boundaries fall on
/// `align`-byte boundaries, as evenly as alignment allows. Empty stripes are dropped.
pub fn stripe(range: Range<u64>, n: usize, align: u64) -> Vec<Range<u64>> {
    assert!(n > 0 && align > 0);
    let len = range.end.saturating_sub(range.start);
    let units = len.div_ceil(align);
    let per = units.div_ceil(n as u64).max(1);
    let mut out = Vec::new();
    let mut at = range.start;
    while at < range.end {
        let end = (at + per * align).min(range.end);
        out.push(at..end);
        at = end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_cover_range_exactly_and_align() {
        let c = chunks(5..30, 8);
        let ranges: Vec<_> = c.iter().map(Chunk::range).collect();
        assert_eq!(ranges, vec![5..8, 8..16, 16..24, 24..30]);
        assert!(c.iter().enumerate().all(|(i, c)| c.index == i));
    }

    #[test]
    fn chunks_of_empty_range_is_empty() {
        assert!(chunks(7..7, 8).is_empty());
    }

    #[test]
    fn stripes_cover_range_and_align() {
        let s = stripe(0..100, 4, 8);
        assert_eq!(s.first().unwrap().start, 0);
        assert_eq!(s.last().unwrap().end, 100);
        for w in s.windows(2) {
            assert_eq!(w[0].end, w[1].start);
            assert_eq!(w[0].end % 8, 0);
        }
        assert!(s.len() <= 4);
    }

    #[test]
    fn more_stripes_than_units_drops_empties() {
        let s = stripe(0..10, 8, 8);
        assert_eq!(s, vec![0..8, 8..10]);
    }
}

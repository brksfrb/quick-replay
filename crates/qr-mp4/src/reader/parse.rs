//! Bounds-checked parsing of in-memory box payloads.

use std::io;

pub(crate) fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

/// Big-endian reader over a byte slice; every read is bounds-checked.
pub(crate) struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub(crate) fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    pub(crate) fn bytes(&mut self, n: usize) -> io::Result<&'a [u8]> {
        if n > self.remaining() {
            return Err(invalid("box payload truncated"));
        }
        let out = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    pub(crate) fn skip(&mut self, n: usize) -> io::Result<()> {
        self.bytes(n).map(|_| ())
    }

    fn array<const N: usize>(&mut self) -> io::Result<[u8; N]> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.bytes(N)?);
        Ok(out)
    }

    pub(crate) fn u8(&mut self) -> io::Result<u8> {
        Ok(self.array::<1>()?[0])
    }

    pub(crate) fn u16(&mut self) -> io::Result<u16> {
        self.array().map(u16::from_be_bytes)
    }

    pub(crate) fn u32(&mut self) -> io::Result<u32> {
        self.array().map(u32::from_be_bytes)
    }

    pub(crate) fn u64(&mut self) -> io::Result<u64> {
        self.array().map(u64::from_be_bytes)
    }

    /// Reads a full-box header and returns the version.
    pub(crate) fn version(&mut self) -> io::Result<u8> {
        let v = self.u8()?;
        self.skip(3)?;
        Ok(v)
    }

    /// Reads a full-box header and returns (version, flags).
    pub(crate) fn version_flags(&mut self) -> io::Result<(u8, u32)> {
        let word = self.u32()?;
        Ok(((word >> 24) as u8, word & 0x00FF_FFFF))
    }

    /// Reads a 32-bit field (version 0) or a 64-bit field (version 1).
    pub(crate) fn uint_v(&mut self, version: u8) -> io::Result<u64> {
        if version == 1 {
            self.u64()
        } else {
            self.u32().map(u64::from)
        }
    }

    /// Reads a table entry count and checks that `count * entry_size` bytes remain,
    /// so callers can allocate safely.
    pub(crate) fn table_len(&mut self, entry_size: usize) -> io::Result<usize> {
        let n = self.u32()? as usize;
        if n.checked_mul(entry_size).is_none_or(|bytes| bytes > self.remaining()) {
            return Err(invalid("table larger than its box"));
        }
        Ok(n)
    }
}

/// One child box inside a parent payload.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RawBox<'a> {
    pub kind: [u8; 4],
    /// Payload without the header.
    pub payload: &'a [u8],
    /// The complete box including its header.
    pub full: &'a [u8],
}

/// Iterates the child boxes of a payload. Handles `size == 1` (64-bit size) and
/// `size == 0` (extends to the end of the parent). Yields one error and stops on
/// malformed headers.
pub(crate) struct BoxIter<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BoxIter<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn parse_next(&mut self) -> io::Result<RawBox<'a>> {
        let rest = &self.data[self.pos..];
        let mut c = Cursor::new(rest);
        let size32 = c.u32()?;
        let mut kind = [0u8; 4];
        kind.copy_from_slice(c.bytes(4)?);
        let (size, header) = match size32 {
            0 => (rest.len() as u64, 8),
            1 => (c.u64()?, 16),
            n => (u64::from(n), 8),
        };
        if size < header || size > rest.len() as u64 {
            return Err(invalid("box size out of range"));
        }
        let size = size as usize;
        self.pos += size;
        Ok(RawBox { kind, payload: &rest[header as usize..size], full: &rest[..size] })
    }
}

impl<'a> Iterator for BoxIter<'a> {
    type Item = io::Result<RawBox<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.data.len() {
            return None;
        }
        // Fewer than 8 trailing bytes cannot hold a box; some muxers pad with zeros.
        if self.data.len() - self.pos < 8 {
            self.pos = self.data.len();
            return None;
        }
        let item = self.parse_next();
        if item.is_err() {
            self.pos = self.data.len();
        }
        Some(item)
    }
}

/// Finds the first child box of `kind`.
pub(crate) fn find_box<'a>(data: &'a [u8], kind: &[u8; 4]) -> io::Result<Option<RawBox<'a>>> {
    for b in BoxIter::new(data) {
        let b = b?;
        if &b.kind == kind {
            return Ok(Some(b));
        }
    }
    Ok(None)
}

/// Finds a required child box.
pub(crate) fn require_box<'a>(data: &'a [u8], kind: &[u8; 4]) -> io::Result<&'a [u8]> {
    find_box(data, kind)?
        .map(|b| b.payload)
        .ok_or_else(|| invalid(&format!("missing '{}' box", String::from_utf8_lossy(kind))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iterates_sizes_one_and_zero() {
        let mut data = Vec::new();
        data.extend_from_slice(&[0, 0, 0, 9, b'a', b'b', b'c', b'd', 7]);
        data.extend_from_slice(&[0, 0, 0, 1, b'l', b'a', b'r', b'g', 0, 0, 0, 0, 0, 0, 0, 18, 1, 2]);
        data.extend_from_slice(&[0, 0, 0, 0, b'r', b'e', b's', b't', 9, 9, 9]);
        let boxes: Vec<RawBox<'_>> = BoxIter::new(&data).map(|b| b.unwrap()).collect();
        assert_eq!(boxes.len(), 3);
        assert_eq!(boxes[0].payload, &[7]);
        assert_eq!(&boxes[1].kind, b"larg");
        assert_eq!(boxes[1].payload, &[1, 2]);
        assert_eq!(boxes[1].full.len(), 18);
        assert_eq!(boxes[2].payload, &[9, 9, 9]);
    }

    #[test]
    fn rejects_bad_sizes() {
        let data = [0, 0, 0, 4, b'a', b'b', b'c', b'd'];
        assert!(BoxIter::new(&data).next().unwrap().is_err());
        let data = [0, 0, 0, 99, b'a', b'b', b'c', b'd'];
        let mut it = BoxIter::new(&data);
        assert!(it.next().unwrap().is_err());
        assert!(it.next().is_none());
        let data = [0, 0, 0, 1, b'a', b'b', b'c', b'd', 0, 0];
        assert!(BoxIter::new(&data).next().unwrap().is_err());
    }

    #[test]
    fn cursor_bounds() {
        assert!(Cursor::new(&[0, 0, 0, 5, 1, 2]).table_len(1).is_err());
        assert!(Cursor::new(&[0xFF, 0xFF, 0xFF, 0xFF]).table_len(8).is_err());
        assert_eq!(Cursor::new(&[0, 0, 0, 1, 1, 2]).table_len(2).unwrap(), 1);
        let mut c = Cursor::new(&[1, 2, 3]);
        assert!(c.u32().is_err());
        assert_eq!(c.u16().unwrap(), 0x0102);
        assert!(c.u16().is_err());
    }
}

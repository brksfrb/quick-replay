//! Big-endian byte buffer with nested box support.

use std::io;

/// Growable buffer for serialising boxes. Box sizes are patched by [`BoxBuf::end`].
#[derive(Debug, Default)]
pub(crate) struct BoxBuf {
    buf: Vec<u8>,
    overflow: bool,
}

impl BoxBuf {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub(crate) fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub(crate) fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub(crate) fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub(crate) fn i64(&mut self, v: i64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    pub(crate) fn bytes(&mut self, v: &[u8]) {
        self.buf.extend_from_slice(v);
    }

    pub(crate) fn zeros(&mut self, n: usize) {
        self.buf.resize(self.buf.len() + n, 0);
    }

    /// Starts a plain box; returns the token to pass to [`BoxBuf::end`].
    pub(crate) fn begin(&mut self, fourcc: &[u8; 4]) -> usize {
        let start = self.buf.len();
        self.u32(0);
        self.bytes(fourcc);
        start
    }

    /// Starts a full box (version + 24-bit flags).
    pub(crate) fn begin_full(&mut self, fourcc: &[u8; 4], version: u8, flags: u32) -> usize {
        let start = self.begin(fourcc);
        self.u32((u32::from(version) << 24) | (flags & 0x00FF_FFFF));
        start
    }

    /// Closes the box started at `start`, patching its 32-bit size.
    pub(crate) fn end(&mut self, start: usize) {
        let size = self.buf.len() - start;
        match u32::try_from(size) {
            Ok(size) => self.buf[start..start + 4].copy_from_slice(&size.to_be_bytes()),
            Err(_) => self.overflow = true,
        }
    }

    /// Returns the bytes, or an error if any box exceeded the 32-bit size limit.
    pub(crate) fn finish(self) -> io::Result<Vec<u8>> {
        if self.overflow {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "box larger than 4 GiB"));
        }
        Ok(self.buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_box_sizes_are_patched() {
        let mut b = BoxBuf::new();
        let outer = b.begin(b"moov");
        let inner = b.begin_full(b"mvhd", 1, 3);
        b.u16(7);
        b.end(inner);
        b.end(outer);
        let v = b.finish().unwrap();
        assert_eq!(&v[0..4], &22u32.to_be_bytes());
        assert_eq!(&v[4..8], b"moov");
        assert_eq!(&v[8..12], &14u32.to_be_bytes());
        assert_eq!(&v[16..20], &[1, 0, 0, 3]);
        assert_eq!(&v[20..22], &[0, 7]);
    }
}

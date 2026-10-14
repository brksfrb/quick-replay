//! RBSP unescaping and an MSB-first bit reader with Exp-Golomb support.

use std::io;

/// Removes emulation-prevention bytes (`00 00 03` -> `00 00`) from a NAL unit.
pub(crate) fn unescape_rbsp(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0usize;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}

fn truncated() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "truncated parameter set")
}

/// MSB-first bit reader over an RBSP buffer.
pub(crate) struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub(crate) fn bit(&mut self) -> io::Result<u32> {
        let byte = self.data.get(self.pos / 8).ok_or_else(truncated)?;
        let bit = (byte >> (7 - (self.pos % 8))) & 1;
        self.pos += 1;
        Ok(u32::from(bit))
    }

    pub(crate) fn flag(&mut self) -> io::Result<bool> {
        Ok(self.bit()? == 1)
    }

    /// Reads `n` (at most 64) bits as an unsigned integer.
    pub(crate) fn bits(&mut self, n: u32) -> io::Result<u64> {
        debug_assert!(n <= 64);
        let mut v = 0u64;
        for _ in 0..n {
            v = (v << 1) | u64::from(self.bit()?);
        }
        Ok(v)
    }

    pub(crate) fn skip(&mut self, n: usize) -> io::Result<()> {
        let end = self.pos.checked_add(n).ok_or_else(truncated)?;
        if end > self.data.len() * 8 {
            return Err(truncated());
        }
        self.pos = end;
        Ok(())
    }

    /// Reads an unsigned Exp-Golomb code (`ue(v)`).
    pub(crate) fn ue(&mut self) -> io::Result<u32> {
        let mut zeros = 0u32;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid Exp-Golomb code"));
            }
        }
        let rest = self.bits(zeros)?;
        let value = (1u64 << zeros) - 1 + rest;
        u32::try_from(value).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Exp-Golomb overflow"))
    }
}

#[cfg(test)]
pub(crate) mod test_util {
    /// MSB-first bit writer used to hand-build parameter sets in tests.
    #[derive(Default)]
    pub(crate) struct BitWriter {
        bytes: Vec<u8>,
        nbits: usize,
    }

    impl BitWriter {
        pub(crate) fn bits(&mut self, n: u32, v: u64) {
            for i in (0..n).rev() {
                if self.nbits.is_multiple_of(8) {
                    self.bytes.push(0);
                }
                let bit = ((v >> i) & 1) as u8;
                let last = self.bytes.len() - 1;
                self.bytes[last] |= bit << (7 - (self.nbits % 8));
                self.nbits += 1;
            }
        }

        pub(crate) fn ue(&mut self, v: u32) {
            let x = u64::from(v) + 1;
            let len = 64 - x.leading_zeros();
            self.bits(len - 1, 0);
            self.bits(len, x);
        }

        /// Appends rbsp trailing bits and returns the bytes.
        pub(crate) fn finish(mut self) -> Vec<u8> {
            self.bits(1, 1);
            while !self.nbits.is_multiple_of(8) {
                self.bits(1, 0);
            }
            self.bytes
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_util::BitWriter;
    use super::*;

    #[test]
    fn unescape_removes_emulation_prevention() {
        assert_eq!(unescape_rbsp(&[0, 0, 3, 1, 0, 0, 3, 0, 0, 3]), vec![0, 0, 1, 0, 0, 0, 0]);
        assert_eq!(unescape_rbsp(&[0, 3, 0, 0, 3]), vec![0, 3, 0, 0]);
    }

    #[test]
    fn reads_exp_golomb_roundtrip() {
        let mut w = BitWriter::default();
        for v in [0u32, 1, 2, 3, 7, 255, 65_535, 1_000_000] {
            w.ue(v);
        }
        w.bits(5, 0b10110);
        let data = w.finish();
        let mut r = BitReader::new(&data);
        for v in [0u32, 1, 2, 3, 7, 255, 65_535, 1_000_000] {
            assert_eq!(r.ue().unwrap(), v);
        }
        assert_eq!(r.bits(5).unwrap(), 0b10110);
        assert!(r.flag().unwrap());
    }

    #[test]
    fn errors_on_truncation() {
        let mut r = BitReader::new(&[0x00]);
        assert!(r.ue().is_err());
        let mut r = BitReader::new(&[0xFF]);
        assert!(r.skip(9).is_err());
        assert!(r.bits(8).is_ok());
        assert!(r.bit().is_err());
    }
}

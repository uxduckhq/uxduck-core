//! Bits of a NAL unit's payload: emulation prevention removed, then read
//! most significant first with Exp-Golomb codes.

/// Removes emulation prevention bytes (the `03` in `00 00 03`).
#[must_use]
pub fn unescape_rbsp(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &byte in nal {
        if zeros >= 2 && byte == 0x03 {
            zeros = 0;
            continue;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        out.push(byte);
    }
    out
}

/// Read past the end, or an Exp-Golomb code too long to be one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutOfBits;

/// Reads bits most significant first.
#[derive(Debug)]
pub struct BitReader<'a> {
    data: &'a [u8],
    bit: usize,
}

impl<'a> BitReader<'a> {
    #[must_use]
    pub const fn new(data: &'a [u8], byte_offset: usize) -> Self {
        Self {
            data,
            bit: byte_offset * 8,
        }
    }

    /// Up to 32 bits as a number.
    ///
    /// # Errors
    /// Reading past the end.
    pub fn u(&mut self, bits: u32) -> Result<u32, OutOfBits> {
        let mut value: u64 = 0;
        for _ in 0..bits {
            let byte = self.data.get(self.bit >> 3).ok_or(OutOfBits)?;
            value = (value << 1) | u64::from((byte >> (7 - (self.bit & 7))) & 1);
            self.bit += 1;
        }
        u32::try_from(value).map_err(|_| OutOfBits)
    }

    pub const fn skip(&mut self, bits: usize) {
        self.bit += bits;
    }

    /// Where the next bit is, counted from the start of the data.
    #[must_use]
    pub const fn position(&self) -> usize {
        self.bit
    }

    /// Bits not yet read.
    #[must_use]
    pub const fn left(&self) -> usize {
        (self.data.len() * 8).saturating_sub(self.bit)
    }

    /// On to the next byte's first bit.
    pub const fn align(&mut self) {
        self.bit = self.bit.next_multiple_of(8);
    }

    /// An unsigned Exp-Golomb code.
    ///
    /// # Errors
    /// Reading past the end, or a code longer than 31 bits.
    pub fn ue(&mut self) -> Result<u32, OutOfBits> {
        let mut zeros = 0;
        while self.u(1)? == 0 {
            zeros += 1;
            if zeros > 31 {
                return Err(OutOfBits);
            }
        }
        let rest = self.u(zeros)?;
        ((1u64 << zeros) - 1 + u64::from(rest))
            .try_into()
            .map_err(|_| OutOfBits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emulation_prevention_comes_out() {
        assert_eq!(unescape_rbsp(&[0, 0, 3, 1, 0, 0, 3, 0]), [0, 0, 1, 0, 0, 0]);
    }

    #[test]
    fn exp_golomb_and_fixed_bits() {
        // 1 | 010 | 011 | 00100 => 0, 1, 2, 3
        let data = [0b1010_0110, 0b0100_0000];
        let mut r = BitReader::new(&data, 0);
        assert_eq!(
            (r.ue(), r.ue(), r.ue(), r.ue()),
            (Ok(0), Ok(1), Ok(2), Ok(3))
        );
        assert_eq!(BitReader::new(&[0xff], 0).u(4), Ok(0xf));
        assert_eq!(BitReader::new(&[0xff], 0).u(9), Err(OutOfBits));
    }
}

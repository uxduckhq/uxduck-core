//! Stable text encodings shared by hashes, manifests and identifiers.

/// Lowercase hexadecimal without allocating an intermediate string.
#[derive(Clone, Copy, Debug)]
pub struct Hex<'a>(pub &'a [u8]);

impl std::fmt::LowerHex for Hex<'_> {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for byte in self.0 {
            write!(out, "{byte:02x}")?;
        }
        Ok(())
    }
}

//! MAVInspect protocol extra types.

use std::fmt::{Display, Formatter};

use base64::Engine;

/// Fingerprint for fast comparison of entities.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fingerprint {
    inner: u64,
}

impl Fingerprint {
    #[inline(always)]
    pub(crate) fn new(value: u64) -> Self {
        Self { inner: value }
    }

    #[inline(always)]
    pub(crate) fn as_bytes(&self) -> [u8; size_of::<Self>()] {
        self.inner.to_le_bytes()
    }
}

impl From<u64> for Fingerprint {
    #[inline(always)]
    fn from(value: u64) -> Self {
        Self::new(value)
    }
}

impl Display for Fingerprint {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!(
            "{}",
            base64::engine::general_purpose::STANDARD_NO_PAD.encode(self.as_bytes())
        ))
    }
}

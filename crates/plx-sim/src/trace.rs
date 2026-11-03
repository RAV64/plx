//! A fingerprint of everything that happened in a run, so two runs of one
//! schedule can be compared: every byte crossing a pipe or a PTY, and every
//! event, in order.

/// FNV-1a over a tagged stream. Not cryptographic; it only has to tell runs
/// apart, deterministically and without allocating.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Trace(u64);

/// What a recorded stretch of bytes was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tag {
    ToDaemon,
    ToClient,
    ToProgram,
    FromProgram,
    ToTerminal,
    Event,
}

impl Trace {
    #[must_use]
    pub const fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    /// Records `bytes` under `tag`, with who they belong to.
    pub fn record(&mut self, tag: Tag, who: u64, bytes: &[u8]) {
        let tag = match tag {
            Tag::ToDaemon => 1,
            Tag::ToClient => 2,
            Tag::ToProgram => 3,
            Tag::FromProgram => 4,
            Tag::ToTerminal => 5,
            Tag::Event => 6,
        };
        self.mix(&[tag]);
        self.mix(&who.to_le_bytes());
        self.mix(&bytes.len().to_le_bytes());
        self.mix(bytes);
    }

    fn mix(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 ^= u64::from(byte);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
}

impl Default for Trace {
    fn default() -> Self {
        Self::new()
    }
}

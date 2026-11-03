//! The fixed handshake: both sides send [`HELLO`] first and check the
//! other's.

/// Changes whenever the wire format changes.
pub const PROTOCOL_VERSION: u32 = 1;

const MAGIC: [u8; 4] = *b"plx\0";

/// The handshake bytes: magic, then the version (little-endian).
pub const HELLO: [u8; 8] = {
    let [v0, v1, v2, v3] = PROTOCOL_VERSION.to_le_bytes();
    let [m0, m1, m2, m3] = MAGIC;
    [m0, m1, m2, m3, v0, v1, v2, v3]
};

/// Why the peer's handshake is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HelloError {
    #[error("the peer is not plx")]
    NotPlx,
    #[error(
        "the peer speaks protocol {theirs}, this plx speaks {}",
        PROTOCOL_VERSION
    )]
    Version { theirs: u32 },
}

/// Checks the peer's handshake.
///
/// # Errors
///
/// When it is not plx's magic, or another protocol version.
pub fn check_hello(peer: [u8; 8]) -> Result<(), HelloError> {
    let [m0, m1, m2, m3, v0, v1, v2, v3] = peer;
    if [m0, m1, m2, m3] != MAGIC {
        return Err(HelloError::NotPlx);
    }
    let theirs = u32::from_le_bytes([v0, v1, v2, v3]);
    if theirs == PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(HelloError::Version { theirs })
    }
}

//! Length-prefixed frames: a `u32` little-endian payload length, then the
//! payload.

use alloc::vec::Vec;
use core::mem;

use plx_bug::OrBug as _;
use serde::Serialize;

use crate::CodecError;

/// The largest message a client may send: a paste of a few megabytes.
pub const MAX_CLIENT_MESSAGE: usize = 4 << 20;

/// The largest message the daemon sends: a whole frame of the largest grid
/// (8 bytes a cell, 2^23 cells) with room for its tables and chrome.
pub const MAX_SERVER_MESSAGE: usize = 80 << 20;

const PREFIX: usize = 4;

/// Appends `message` to `out` as one frame.
///
/// # Errors
///
/// When it cannot be serialised, or exceeds `max`.
pub(crate) fn write<T: Serialize + ?Sized>(
    message: &T,
    max: usize,
    out: &mut Vec<u8>,
) -> Result<(), CodecError> {
    let start = out.len();
    let mut buf = mem::take(out);
    buf.extend_from_slice(&[0; PREFIX]);
    let encoded = postcard::to_extend(message, buf);
    let mut buf = match encoded {
        Ok(buf) => buf,
        Err(error) => return Err(CodecError::Postcard(error)),
    };
    let len = buf.len() - start - PREFIX;
    if len > max {
        buf.truncate(start);
        *out = buf;
        return Err(CodecError::TooLarge { len, max });
    }
    let prefix = u32::try_from(len)
        .or_bug("message maxima fit u32")
        .to_le_bytes();
    buf.get_mut(start..start + PREFIX)
        .or_bug("the prefix was reserved above")
        .copy_from_slice(&prefix);
    *out = buf;
    Ok(())
}

/// The first complete frame's payload at the start of `buf`, and the bytes
/// it takes (prefix included); `None` while it is incomplete. Bytes may
/// arrive in any chunks: call again with more.
///
/// # Errors
///
/// When the announced length exceeds `max`, before anything is buffered.
pub fn split(buf: &[u8], max: usize) -> Result<Option<(&[u8], usize)>, CodecError> {
    let Some(prefix) = buf.first_chunk::<PREFIX>() else {
        return Ok(None);
    };
    let len = usize::try_from(u32::from_le_bytes(*prefix)).or_bug("u32 fits usize");
    if len > max {
        return Err(CodecError::TooLarge { len, max });
    }
    Ok(buf
        .get(PREFIX..PREFIX + len)
        .map(|payload| (payload, PREFIX + len)))
}

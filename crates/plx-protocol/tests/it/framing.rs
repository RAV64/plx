use plx_protocol::CodecError;
use plx_protocol::framing::split;
use plx_protocol::hello::{HELLO, HelloError, PROTOCOL_VERSION, check_hello};

#[test]
fn the_handshake_accepts_only_this_protocol() {
    assert_eq!(check_hello(HELLO), Ok(()));
    let mut other = HELLO;
    other[4..].copy_from_slice(&(PROTOCOL_VERSION + 1).to_le_bytes());
    assert_eq!(
        check_hello(other),
        Err(HelloError::Version {
            theirs: PROTOCOL_VERSION + 1
        })
    );
    assert_eq!(check_hello(*b"SSH-2.0-"), Err(HelloError::NotPlx));
}

#[test]
fn frames_split_only_when_complete() {
    let frame = [3, 0, 0, 0, b'a', b'b', b'c', 9];
    assert!(matches!(split(&frame[..2], 100), Ok(None)));
    assert!(matches!(split(&frame[..6], 100), Ok(None)));
    let (payload, used) = split(&frame, 100).unwrap().unwrap();
    assert_eq!((payload, used), (&b"abc"[..], 7));
}

#[test]
fn an_oversized_frame_is_refused_from_its_prefix_alone() {
    let prefix = 1_000_u32.to_le_bytes();
    assert!(matches!(
        split(&prefix, 999),
        Err(CodecError::TooLarge {
            len: 1000,
            max: 999
        })
    ));
}

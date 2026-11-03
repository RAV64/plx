//! What a client may write to its terminal.

use plx_sim::oracle::in_vocabulary;

/// The client's own clipboard write passes; a pane's sequences that only
/// the raw stream could carry do not.
#[test]
fn only_the_clients_own_clipboard_write_passes() {
    assert_eq!(in_vocabulary(b"\x1b]52;c;aGVsbG8=\x1b\\text"), Ok(()));
    for raw in [
        &b"\x1b]52;c;aGVsbG8=\x07"[..],
        b"\x1b]52;p;aGVsbG8=\x1b\\",
        b"\x1b]52;c;?\x1b\\",
        b"\x1b]0;a title\x1b\\",
        b"\x1b]8;;https://example.com\x1b\\",
        b"\x1bc",
    ] {
        assert!(in_vocabulary(raw).is_err(), "{raw:?}");
    }
}

/// The client's own progress reports pass (ST-ended, a known state, a
/// percentage only where the state has one, at most 100); anything else
/// in OSC 9 does not.
#[test]
fn only_the_clients_own_progress_reports_pass() {
    for ours in [
        &b"\x1b]9;4;0\x1b\\"[..],
        b"\x1b]9;4;1;42\x1b\\",
        b"\x1b]9;4;1\x1b\\",
        b"\x1b]9;4;2;100\x1b\\",
        b"\x1b]9;4;3\x1b\\",
        b"\x1b]9;4;4;0\x1b\\",
    ] {
        assert_eq!(in_vocabulary(ours), Ok(()), "{ours:?}");
    }
    for raw in [
        &b"\x1b]9;4;1;42\x07"[..],
        b"\x1b]9;4;1;250\x1b\\",
        b"\x1b]9;4;3;50\x1b\\",
        b"\x1b]9;4;7\x1b\\",
        b"\x1b]9;4;\x1b\\",
        b"\x1b]9;a notification\x1b\\",
    ] {
        assert!(in_vocabulary(raw).is_err(), "{raw:?}");
    }
}

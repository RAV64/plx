use core::num::{NonZeroU16, NonZeroU32};

use plx_core::command::{ClientCommand, Command, SessionCommand};
use plx_core::geometry::Direction;
use plx_core::message::{FrameSeq, Paste};
use plx_core::tab::{TabName, TabNumber};
use plx_emulator::{CellPx, GridSize, Key, KeyAction, KeyChar, KeyEvent, Mods};
use plx_protocol::CodecError;
use plx_protocol::client::{ClientWire, decode, encode};
use plx_protocol::framing::{MAX_CLIENT_MESSAGE, split};
use proptest::prelude::*;

fn size() -> impl Strategy<Value = GridSize> {
    (1_u16..=GridSize::MAX_COLS, 1_u16..=GridSize::MAX_ROWS).prop_map(|(c, r)| {
        GridSize::new(NonZeroU16::new(c).unwrap(), NonZeroU16::new(r).unwrap()).unwrap()
    })
}

fn cell_px() -> impl Strategy<Value = Option<CellPx>> {
    proptest::option::of((1_u32..100, 1_u32..100).prop_map(|(w, h)| CellPx {
        width: NonZeroU32::new(w).unwrap(),
        height: NonZeroU32::new(h).unwrap(),
    }))
}

/// A command, built from generated parts (commands are not `Clone`).
fn command(kind: u8, name: &str, tab: u16) -> Command {
    match kind % 5 {
        0 => Command::Client(ClientCommand::Detach),
        1 => Command::Session(SessionCommand::NewTab),
        2 => Command::Session(SessionCommand::Split {
            direction: Direction::Down,
        }),
        3 => Command::Session(SessionCommand::FocusTab {
            tab: TabNumber::new(NonZeroU16::new(tab.max(1)).unwrap()),
        }),
        _ => Command::Session(SessionCommand::RenameTab {
            name: TabName::new(name).unwrap(),
        }),
    }
}

/// A message borrowing `paste` (the generated text lives in the test).
fn message(
    kind: u8,
    paste: &str,
    size: GridSize,
    cell_px: Option<CellPx>,
    command: Command,
    acks: u8,
) -> ClientWire<'_> {
    match kind % 6 {
        0 => ClientWire::Attach { size, cell_px },
        1 => ClientWire::Resize { size, cell_px },
        2 => ClientWire::Key(KeyEvent {
            key: Key::Char(KeyChar::new('x').unwrap()),
            mods: Mods::CTRL,
            action: KeyAction::Repeat,
        }),
        3 => ClientWire::Paste(Paste::new(paste)),
        4 => ClientWire::Command(command),
        _ => ClientWire::Ack((0..acks).fold(FrameSeq::FIRST, |seq, _| seq.next())),
    }
}

proptest! {
    /// Every message survives encoding, any chunking of the byte stream, and
    /// decoding, in order.
    #[test]
    fn messages_round_trip_through_any_chunking(
        parts in proptest::collection::vec(
            (any::<u8>(), "\\PC{0,40}", size(), cell_px(), (any::<u8>(), "[a-z]{1,10}", any::<u16>()), any::<u8>()),
            1..8,
        ),
        cuts in proptest::collection::vec(any::<usize>(), 0..6),
    ) {
        let messages: Vec<ClientWire<'_>> = parts
            .iter()
            .map(|(kind, paste, size, px, (command_kind, name, tab), acks)| {
                message(*kind, paste, *size, *px, command(*command_kind, name, *tab), *acks)
            })
            .collect();
        let mut stream = Vec::new();
        for message in &messages {
            encode(message, &mut stream).unwrap();
        }
        // Feed the stream in arbitrary chunks into a receive buffer.
        let mut points: Vec<usize> = cuts.iter().map(|c| c % (stream.len() + 1)).collect();
        points.push(stream.len());
        points.sort_unstable();
        let mut received = Vec::new();
        let mut buffer: Vec<u8> = Vec::new();
        let mut start = 0;
        for end in points {
            buffer.extend_from_slice(&stream[start..end]);
            start = end;
            while let Some((payload, used)) = split(&buffer, MAX_CLIENT_MESSAGE).unwrap() {
                received.push(format!("{:?}", decode(payload).unwrap()));
                buffer.drain(..used);
            }
        }
        let expected: Vec<String> = messages.iter().map(|m| format!("{m:?}")).collect();
        prop_assert_eq!(received, expected);
        prop_assert!(buffer.is_empty());
    }

    /// Garbage never panics the decoder: it is an error or a valid message.
    #[test]
    fn garbage_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..64)) {
        let _result = decode(&bytes);
    }
}

#[test]
fn bytes_after_a_message_are_refused() {
    let mut frame = Vec::new();
    encode(&ClientWire::Ack(FrameSeq::FIRST), &mut frame).unwrap();
    let (payload, _) = split(&frame, MAX_CLIENT_MESSAGE).unwrap().unwrap();
    let mut longer = payload.to_vec();
    longer.push(0);
    assert!(matches!(decode(&longer), Err(CodecError::Trailing(1))));
}

#[test]
fn invalid_values_are_refused_at_the_boundary() {
    // An Attach whose size exceeds the grid limits: variant 0, then the
    // size as two varints (65535, 65535), then no cell size.
    let payload = [0, 0xff, 0xff, 0x03, 0xff, 0xff, 0x03, 0];
    assert!(matches!(decode(&payload), Err(CodecError::Postcard(_))));
    // A key typing a platform function-key code (U+F700).
    let key = ClientWire::Key(KeyEvent::press(
        Key::Char(KeyChar::new('a').unwrap()),
        Mods::empty(),
    ));
    let mut frame = Vec::new();
    encode(&key, &mut frame).unwrap();
    let (payload, _) = split(&frame, MAX_CLIENT_MESSAGE).unwrap().unwrap();
    let forged: Vec<u8> = {
        let mut bytes = payload.to_vec();
        // The char is UTF-8 with a length prefix: replace 'a' with U+F700.
        let at = bytes.iter().position(|&b| b == b'a').unwrap();
        bytes.splice(at - 1..=at, [3, 0xEF, 0x9C, 0x80]);
        bytes
    };
    assert!(decode(&forged).is_err());
}

/// The wire format is fixed: these bytes must never change without a
/// protocol version bump.
#[test]
fn golden_bytes() {
    let mut frame = Vec::new();
    encode(&ClientWire::Paste(Paste::new("hi")), &mut frame).unwrap();
    assert_eq!(frame, [4, 0, 0, 0, 3, 2, b'h', b'i']);
    frame.clear();
    encode(&ClientWire::Ack(FrameSeq::FIRST.next()), &mut frame).unwrap();
    assert_eq!(frame, [2, 0, 0, 0, 5, 1]);
}

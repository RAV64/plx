//! Daemon to client: frames, rejections, detach and session end.
//!
//! A frame is serialised straight from the core's borrowed `FrameView`, and
//! applied on the client straight into its mirror through [`FrameApply`]:
//! nothing in between is materialised. Its layout (postcard tuples and
//! length-prefixed sequences):
//!
//! ```text
//! frame  = seq, size, mode, active tab (u16), focus, focus scroll,
//!          focus progress?, (needle, search status)?, [(tab name?, zoom)], [(pane, rect?)],
//!          [(separator rect, axis, split)], [pane]
//! pane   = id, rect, cursor, [(style id, style)], [(cluster id, [char])],
//!          [(y, cell bytes)]
//! ```
//!
//! A row travels as one byte block: its cells' 8-byte wire forms, which are
//! their memory form, so encoding borrows the grid row
//! ([`Cell::slice_as_wire`]) and decoding checks each cell straight into
//! the receiver's row ([`FrameApply::row_slot`]).

use alloc::vec::Vec;
use core::fmt;
use core::marker::PhantomData;

use plx_bug::OrBug as _;
use plx_core::event::EndReason;
use plx_core::geometry::{Axis, Rect};
use plx_core::id::PaneId;
use plx_core::layout::{Placement, Separator, SplitIndex};
use plx_core::message::{FrameSeq, FrameView, Outgoing, PaneUpdate};
use plx_core::mode::Mode;
use plx_core::rejection::Rejection;
use plx_core::tab::{TabName, Zoom};
use plx_emulator::{
    Cell, Cursor, Emulator, GraphemeId, GridSize, Progress, ScrollPosition, SearchStatus, Style,
    StyleId,
};
use serde::de::{self, DeserializeSeed, EnumAccess, SeqAccess, VariantAccess, Visitor};
use serde::ser::{SerializeSeq, SerializeTuple};
use serde::{Deserializer, Serialize, Serializer};

use crate::CodecError;
use crate::framing::{self, MAX_SERVER_MESSAGE};

const NAME: &str = "Server";
const VARIANTS: &[&str] = &[
    "Frame",
    "Rejected",
    "Detached",
    "SessionEnded",
    "Applied",
    "Clipboard",
];

/// Appends `message` to `out` (the client's send buffer) as one frame.
///
/// # Errors
///
/// When it exceeds [`MAX_SERVER_MESSAGE`].
pub fn encode_outgoing<E: Emulator>(
    message: &Outgoing<'_, E>,
    out: &mut Vec<u8>,
) -> Result<(), CodecError> {
    framing::write(&OutgoingWire(message), MAX_SERVER_MESSAGE, out)
}

struct OutgoingWire<'m, 'a, E>(&'m Outgoing<'a, E>);

impl<E: Emulator> Serialize for OutgoingWire<'_, '_, E> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            Outgoing::Frame(view) => {
                s.serialize_newtype_variant(NAME, 0, "Frame", &FrameWire(view))
            }
            Outgoing::Rejected(rejection) => {
                s.serialize_newtype_variant(NAME, 1, "Rejected", rejection)
            }
            Outgoing::Detached => s.serialize_unit_variant(NAME, 2, "Detached"),
            Outgoing::SessionEnded(reason) => {
                s.serialize_newtype_variant(NAME, 3, "SessionEnded", reason)
            }
            Outgoing::Applied => s.serialize_unit_variant(NAME, 4, "Applied"),
            Outgoing::Clipboard(copied) => {
                s.serialize_newtype_variant(NAME, 5, "Clipboard", &Bytes(copied.bytes()))
            }
        }
    }
}

struct FrameWire<'v, 'a, E>(&'v FrameView<'a, E>);

impl<E: Emulator> Serialize for FrameWire<'_, '_, E> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let view = self.0;
        let active =
            u16::try_from(view.active_position()).or_bug("tab positions are bounded by MAX_TABS");
        let mut t = s.serialize_tuple(FRAME_FIELDS)?;
        t.serialize_element(&view.seq())?;
        t.serialize_element(&view.size())?;
        t.serialize_element(&view.mode())?;
        t.serialize_element(&active)?;
        t.serialize_element(&view.focus())?;
        t.serialize_element(&view.scroll())?;
        t.serialize_element(&view.progress_or_none())?;
        t.serialize_element(
            &view
                .search()
                .map(|search| (search.needle.as_str(), search.status)),
        )?;
        t.serialize_element(&Counted(|| {
            view.tabs()
                .map(|tab| (tab.name().map(TabName::as_str), tab.zoom()))
        }))?;
        let arrangement = view.arrangement();
        t.serialize_element(&Counted(|| {
            arrangement.panes.iter().map(|p| (p.pane, p.rect))
        }))?;
        t.serialize_element(&Counted(|| {
            arrangement
                .separators
                .iter()
                .map(|s| (s.rect, s.axis, s.split))
        }))?;
        t.serialize_element(&Counted(|| view.panes().map(PaneWire)))?;
        t.end()
    }
}

/// A sequence whose length is counted by walking it once before it is
/// written (postcard prefixes every sequence with its length).
struct Counted<F>(F);

impl<F, I> Serialize for Counted<F>
where
    F: Fn() -> I,
    I: Iterator,
    I::Item: Serialize,
{
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some((self.0)().count()))?;
        for item in (self.0)() {
            seq.serialize_element(&item)?;
        }
        seq.end()
    }
}

struct PaneWire<'a>(PaneUpdate<'a>);

impl Serialize for PaneWire<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let update = &self.0;
        let mut t = s.serialize_tuple(6)?;
        t.serialize_element(&update.pane)?;
        t.serialize_element(&update.rect)?;
        t.serialize_element(&update.cursor())?;
        t.serialize_element(&Counted(|| update.styles()))?;
        t.serialize_element(&Counted(|| update.graphemes()))?;
        t.serialize_element(&Counted(|| {
            update
                .rows()
                .map(|(y, cells)| (y, Bytes(Cell::slice_as_wire(cells))))
        }))?;
        t.end()
    }
}

/// Bytes as one block (serde serialises a plain `[u8]` element by element).
struct Bytes<'a>(&'a [u8]);

impl Serialize for Bytes<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bytes(self.0)
    }
}

/// A frame's fixed part.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameHeader {
    pub seq: FrameSeq,
    pub size: GridSize,
    pub mode: Mode,
    /// The active tab's zero-based position.
    pub active: u16,
    pub focus: PaneId,
    /// Where the focused pane's view is in its history.
    pub scroll: ScrollPosition,
    /// The focused pane's progress report, for the client's terminal, when
    /// it has not been told it.
    pub progress: Option<Progress>,
}

/// The fields of a frame's tuple.
const FRAME_FIELDS: usize = 12;

/// The search going on, as a frame carries it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameSearch<'a> {
    pub needle: NeedleText<'a>,
    pub status: SearchStatus,
}

/// A search needle as it arrives, before the receiver checks it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NeedleText<'a>(&'a str);

impl<'a> NeedleText<'a> {
    #[must_use]
    pub const fn as_str(&self) -> &'a str {
        self.0
    }
}

/// Why a client refuses a frame that decodes but does not fit its state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refused(pub &'static str);

/// Receives a frame's parts in order, as they are decoded. The client
/// mirror implements it.
///
/// After any error the receiver may hold part of the frame (rows written
/// but not checked among them) and must be discarded.
pub trait FrameApply {
    /// The frame starts, with the search going on if any; tabs, placements
    /// and separators follow in full.
    ///
    /// # Errors
    ///
    /// When the frame does not fit the receiver.
    fn begin(
        &mut self,
        header: FrameHeader,
        search: Option<FrameSearch<'_>>,
    ) -> Result<(), Refused>;
    /// The next tab, by its name if it has one.
    ///
    /// # Errors
    ///
    /// As [`FrameApply::begin`].
    fn tab(&mut self, name: Option<&str>, zoom: Zoom) -> Result<(), Refused>;
    /// # Errors
    ///
    /// As [`FrameApply::begin`].
    fn placement(&mut self, placement: Placement) -> Result<(), Refused>;
    /// # Errors
    ///
    /// As [`FrameApply::begin`].
    fn separator(&mut self, separator: Separator) -> Result<(), Refused>;
    /// A visible pane's update starts; its styles, clusters and rows follow.
    ///
    /// # Errors
    ///
    /// As [`FrameApply::begin`].
    fn pane(&mut self, pane: PaneId, rect: Rect, cursor: Cursor) -> Result<(), Refused>;
    /// # Errors
    ///
    /// As [`FrameApply::begin`].
    fn style(&mut self, id: StyleId, style: Style) -> Result<(), Refused>;
    /// # Errors
    ///
    /// As [`FrameApply::begin`].
    fn grapheme(&mut self, id: GraphemeId, codepoints: &[char]) -> Result<(), Refused>;
    /// Lends row `y` of the current pane for the decoder to overwrite with
    /// the row's cells, each a valid `Cell`; it must be exactly as wide as
    /// the pane. [`FrameApply::row_filled`] follows.
    ///
    /// # Errors
    ///
    /// As [`FrameApply::begin`].
    fn row_slot(&mut self, y: u16) -> Result<&mut [Cell], Refused>;
    /// Row `y` now holds the frame's cells; the receiver checks what they
    /// refer to.
    ///
    /// # Errors
    ///
    /// As [`FrameApply::begin`].
    fn row_filled(&mut self, y: u16) -> Result<(), Refused>;
    /// The frame is complete.
    ///
    /// # Errors
    ///
    /// As [`FrameApply::begin`].
    fn end(&mut self) -> Result<(), Refused>;
    /// Text the client selected (UTF-8), for its terminal's clipboard (not
    /// part of a frame).
    ///
    /// # Errors
    ///
    /// When the receiver takes no clipboard.
    fn clipboard(&mut self, text: &[u8]) -> Result<(), Refused>;
}

/// The receiver of a client that has not attached (the CLI's one command
/// per connection): it is sent no frames, so it refuses any.
#[derive(Clone, Copy, Debug)]
pub struct Unattached;

impl Unattached {
    const REFUSED: Refused = Refused("an unattached client is sent no frames");
}

impl FrameApply for Unattached {
    fn begin(&mut self, _: FrameHeader, _: Option<FrameSearch<'_>>) -> Result<(), Refused> {
        Err(Self::REFUSED)
    }
    fn tab(&mut self, _: Option<&str>, _: Zoom) -> Result<(), Refused> {
        Err(Self::REFUSED)
    }
    fn placement(&mut self, _: Placement) -> Result<(), Refused> {
        Err(Self::REFUSED)
    }
    fn separator(&mut self, _: Separator) -> Result<(), Refused> {
        Err(Self::REFUSED)
    }
    fn pane(&mut self, _: PaneId, _: Rect, _: Cursor) -> Result<(), Refused> {
        Err(Self::REFUSED)
    }
    fn style(&mut self, _: StyleId, _: Style) -> Result<(), Refused> {
        Err(Self::REFUSED)
    }
    fn grapheme(&mut self, _: GraphemeId, _: &[char]) -> Result<(), Refused> {
        Err(Self::REFUSED)
    }
    fn row_slot(&mut self, _: u16) -> Result<&mut [Cell], Refused> {
        Err(Self::REFUSED)
    }
    fn row_filled(&mut self, _: u16) -> Result<(), Refused> {
        Err(Self::REFUSED)
    }
    fn end(&mut self) -> Result<(), Refused> {
        Err(Self::REFUSED)
    }
    fn clipboard(&mut self, _: &[u8]) -> Result<(), Refused> {
        Err(Refused("an unattached client is sent no clipboard"))
    }
}

/// A daemon message, after a frame's parts went to the [`FrameApply`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerMessage {
    Frame(FrameSeq),
    Rejected(Rejection),
    Detached,
    SessionEnded(EndReason),
    /// The command this (unattached) client sent was applied.
    Applied,
    /// Text for the clipboard, handed to [`FrameApply::clipboard`].
    Clipboard,
}

/// Decodes daemon messages, reusing its buffers: steady-state decoding
/// allocates nothing.
#[derive(Debug, Default)]
pub struct Decoder {
    chars: Vec<char>,
}

impl Decoder {
    #[must_use]
    pub const fn new() -> Self {
        Self { chars: Vec::new() }
    }

    /// Decodes one frame's payload (from [`framing::split`]), handing a
    /// frame's parts to `apply` as they come.
    ///
    /// # Errors
    ///
    /// When it is malformed, has trailing bytes, or `apply` refuses it.
    pub fn decode<A: FrameApply>(
        &mut self,
        payload: &[u8],
        apply: &mut A,
    ) -> Result<ServerMessage, CodecError> {
        let mut refused = None;
        let mut deserializer = postcard::Deserializer::from_bytes(payload);
        let mut ctx = Ctx {
            apply,
            chars: &mut self.chars,
            refused: &mut refused,
        };
        match ServerSeed(&mut ctx).deserialize(&mut deserializer) {
            Ok(message) => {
                let rest = deserializer.finalize()?;
                if rest.is_empty() {
                    Ok(message)
                } else {
                    Err(CodecError::Trailing(rest.len()))
                }
            }
            Err(error) => Err(refused.map_or(CodecError::Postcard(error), CodecError::Invalid)),
        }
    }
}

/// What every seed borrows: the receiver and the decoder's buffers.
struct Ctx<'c, A> {
    apply: &'c mut A,
    chars: &'c mut Vec<char>,
    refused: &'c mut Option<Refused>,
}

impl<A> Ctx<'_, A> {
    /// Passes a refusal on as a decoding error, keeping its reason.
    fn check<E: de::Error>(&mut self, result: Result<(), Refused>) -> Result<(), E> {
        result.map_err(|refused| refuse(self.refused, refused))
    }
}

/// Records a refusal and turns it into a decoding error that keeps its
/// reason.
fn refuse<E: de::Error>(slot: &mut Option<Refused>, refused: Refused) -> E {
    *slot = Some(refused);
    E::custom(refused.0)
}

fn next<'de, T: de::Deserialize<'de>, S: SeqAccess<'de>>(
    seq: &mut S,
    index: usize,
) -> Result<T, S::Error> {
    seq.next_element()?
        .ok_or_else(|| de::Error::invalid_length(index, &"a complete frame"))
}

fn next_seed<'de, T: DeserializeSeed<'de>, S: SeqAccess<'de>>(
    seq: &mut S,
    seed: T,
    index: usize,
) -> Result<T::Value, S::Error> {
    seq.next_element_seed(seed)?
        .ok_or_else(|| de::Error::invalid_length(index, &"a complete frame"))
}

struct ServerSeed<'s, 'c, A>(&'s mut Ctx<'c, A>);

impl<'de, A: FrameApply> DeserializeSeed<'de> for ServerSeed<'_, '_, A> {
    type Value = ServerMessage;

    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<ServerMessage, D::Error> {
        d.deserialize_enum(NAME, VARIANTS, self)
    }
}

impl<'de, A: FrameApply> Visitor<'de> for ServerSeed<'_, '_, A> {
    type Value = ServerMessage;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a daemon message")
    }

    fn visit_enum<E: EnumAccess<'de>>(self, data: E) -> Result<ServerMessage, E::Error> {
        let (index, variant) = data.variant::<u32>()?;
        match index {
            0 => variant
                .newtype_variant_seed(FrameSeed(self.0))
                .map(ServerMessage::Frame),
            1 => variant.newtype_variant().map(ServerMessage::Rejected),
            2 => variant.unit_variant().map(|()| ServerMessage::Detached),
            3 => variant.newtype_variant().map(ServerMessage::SessionEnded),
            4 => variant.unit_variant().map(|()| ServerMessage::Applied),
            5 => {
                let text: &'de [u8] = variant.newtype_variant()?;
                let copied = self.0.apply.clipboard(text);
                self.0.check(copied)?;
                Ok(ServerMessage::Clipboard)
            }
            other => Err(de::Error::invalid_value(
                de::Unexpected::Unsigned(u64::from(other)),
                &"a daemon message variant",
            )),
        }
    }
}

struct FrameSeed<'s, 'c, A>(&'s mut Ctx<'c, A>);

impl<'de, A: FrameApply> DeserializeSeed<'de> for FrameSeed<'_, '_, A> {
    type Value = FrameSeq;

    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<FrameSeq, D::Error> {
        d.deserialize_tuple(FRAME_FIELDS, self)
    }
}

impl<'de, A: FrameApply> Visitor<'de> for FrameSeed<'_, '_, A> {
    type Value = FrameSeq;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a frame")
    }

    fn visit_seq<S: SeqAccess<'de>>(self, mut seq: S) -> Result<FrameSeq, S::Error> {
        let ctx = self.0;
        let (seq_number, size, mode, active, focus, scroll) = (
            next(&mut seq, 0)?,
            next(&mut seq, 1)?,
            next(&mut seq, 2)?,
            next(&mut seq, 3)?,
            next(&mut seq, 4)?,
            next(&mut seq, 5)?,
        );
        let progress = next(&mut seq, 6)?;
        let search: Option<(&'de str, SearchStatus)> = next(&mut seq, 7)?;
        let header = FrameHeader {
            seq: seq_number,
            size,
            mode,
            active,
            focus,
            scroll,
            progress,
        };
        let begun = ctx.apply.begin(
            header,
            search.map(|(needle, status)| FrameSearch {
                needle: NeedleText(needle),
                status,
            }),
        );
        ctx.check(begun)?;
        next_seed(&mut seq, List(&mut *ctx, Part::Tabs), 8)?;
        next_seed(&mut seq, List(&mut *ctx, Part::Placements), 9)?;
        next_seed(&mut seq, List(&mut *ctx, Part::Separators), 10)?;
        next_seed(&mut seq, List(&mut *ctx, Part::Panes), 11)?;
        let ended = ctx.apply.end();
        ctx.check(ended)?;
        Ok(header.seq)
    }
}

/// Which list a [`List`] seed decodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Part {
    Tabs,
    Placements,
    Separators,
    Panes,
    Styles,
    Graphemes,
    Rows,
}

/// A length-prefixed list of one [`Part`], each element handed on.
struct List<'s, 'c, A>(&'s mut Ctx<'c, A>, Part);

impl<'de, A: FrameApply> DeserializeSeed<'de> for List<'_, '_, A> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_seq(self)
    }
}

impl<'de, A: FrameApply> Visitor<'de> for List<'_, '_, A> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "a list of {:?}", self.1)
    }

    fn visit_seq<S: SeqAccess<'de>>(self, mut seq: S) -> Result<(), S::Error> {
        let List(ctx, part) = self;
        loop {
            let applied = match part {
                Part::Tabs => match seq.next_element::<(Option<&'de str>, Zoom)>()? {
                    Some((name, zoom)) => ctx.apply.tab(name, zoom),
                    None => return Ok(()),
                },
                Part::Placements => match seq.next_element::<(PaneId, Option<Rect>)>()? {
                    Some((pane, rect)) => ctx.apply.placement(Placement { pane, rect }),
                    None => return Ok(()),
                },
                Part::Separators => match seq.next_element::<(Rect, Axis, SplitIndex)>()? {
                    Some((rect, axis, split)) => {
                        ctx.apply.separator(Separator { rect, axis, split })
                    }
                    None => return Ok(()),
                },
                Part::Styles => match seq.next_element::<(StyleId, Style)>()? {
                    Some((id, style)) => ctx.apply.style(id, style),
                    None => return Ok(()),
                },
                Part::Panes => match seq.next_element_seed(PaneSeed(&mut *ctx))? {
                    Some(()) => Ok(()),
                    None => return Ok(()),
                },
                Part::Graphemes => match seq.next_element_seed(GraphemeSeed(&mut *ctx))? {
                    Some(()) => Ok(()),
                    None => return Ok(()),
                },
                Part::Rows => match seq.next_element_seed(RowSeed(&mut *ctx))? {
                    Some(()) => Ok(()),
                    None => return Ok(()),
                },
            };
            ctx.check(applied)?;
        }
    }
}

struct PaneSeed<'s, 'c, A>(&'s mut Ctx<'c, A>);

impl<'de, A: FrameApply> DeserializeSeed<'de> for PaneSeed<'_, '_, A> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_tuple(6, self)
    }
}

impl<'de, A: FrameApply> Visitor<'de> for PaneSeed<'_, '_, A> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a pane update")
    }

    fn visit_seq<S: SeqAccess<'de>>(self, mut seq: S) -> Result<(), S::Error> {
        let ctx = self.0;
        let pane = next(&mut seq, 0)?;
        let rect = next(&mut seq, 1)?;
        let cursor = next(&mut seq, 2)?;
        let begun = ctx.apply.pane(pane, rect, cursor);
        ctx.check(begun)?;
        next_seed(&mut seq, List(&mut *ctx, Part::Styles), 3)?;
        next_seed(&mut seq, List(&mut *ctx, Part::Graphemes), 4)?;
        next_seed(&mut seq, List(&mut *ctx, Part::Rows), 5)?;
        Ok(())
    }
}

/// `(cluster id, [char])`, the characters gathered in the decoder's buffer.
struct GraphemeSeed<'s, 'c, A>(&'s mut Ctx<'c, A>);

impl<'de, A: FrameApply> DeserializeSeed<'de> for GraphemeSeed<'_, '_, A> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_tuple(2, self)
    }
}

impl<'de, A: FrameApply> Visitor<'de> for GraphemeSeed<'_, '_, A> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a grapheme cluster")
    }

    fn visit_seq<S: SeqAccess<'de>>(self, mut seq: S) -> Result<(), S::Error> {
        let ctx = self.0;
        let id: GraphemeId = next(&mut seq, 0)?;
        ctx.chars.clear();
        next_seed(
            &mut seq,
            Gather(&mut *ctx.chars, |c: char| Ok(c), PhantomData),
            1,
        )?;
        let applied = ctx.apply.grapheme(id, ctx.chars);
        ctx.check(applied)
    }
}

/// `(y, cell bytes)`, the bytes borrowed from the payload and each cell
/// checked straight into the receiver's row.
struct RowSeed<'s, 'c, A>(&'s mut Ctx<'c, A>);

impl<'de, A: FrameApply> DeserializeSeed<'de> for RowSeed<'_, '_, A> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_tuple(2, self)
    }
}

impl<'de, A: FrameApply> Visitor<'de> for RowSeed<'_, '_, A> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a row")
    }

    fn visit_seq<S: SeqAccess<'de>>(self, mut seq: S) -> Result<(), S::Error> {
        let ctx = self.0;
        let y: u16 = next(&mut seq, 0)?;
        let bytes: &'de [u8] = next(&mut seq, 1)?;
        let (wire, partial) = bytes.as_chunks::<{ Cell::BYTES }>();
        if !partial.is_empty() {
            return Err(de::Error::custom("a row of whole cells"));
        }
        let slot = ctx
            .apply
            .row_slot(y)
            .map_err(|refused| refuse(ctx.refused, refused))?;
        if slot.len() != wire.len() {
            return Err(refuse(ctx.refused, Refused("a row as wide as its pane")));
        }
        Cell::copy_from_wire(slot, wire)
            .map_err(|_invalid| de::Error::custom("a row holds an invalid cell"))?;
        let filled = ctx.apply.row_filled(y);
        ctx.check(filled)
    }
}

/// A list gathered into a reused buffer, each wire element (`W`) converted.
struct Gather<'b, T, W, F>(&'b mut Vec<T>, F, PhantomData<fn(W)>);

impl<'de, T, W, F> DeserializeSeed<'de> for Gather<'_, T, W, F>
where
    W: de::Deserialize<'de>,
    F: Fn(W) -> Result<T, &'static str>,
{
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_seq(self)
    }
}

impl<'de, T, W, F> Visitor<'de> for Gather<'_, T, W, F>
where
    W: de::Deserialize<'de>,
    F: Fn(W) -> Result<T, &'static str>,
{
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a list")
    }

    fn visit_seq<S: SeqAccess<'de>>(self, mut seq: S) -> Result<(), S::Error> {
        let Gather(buf, convert, PhantomData) = self;
        while let Some(wire) = seq.next_element::<W>()? {
            buf.push(convert(wire).map_err(de::Error::custom)?);
        }
        Ok(())
    }
}

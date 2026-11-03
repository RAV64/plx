//! The compact cell: 8 bytes, `Copy`, no heap.

use core::fmt;

use plx_bug::OrBug as _;
use serde::{Deserialize, Serialize};

/// What a cell shows: nothing, one Unicode scalar value, or a grapheme
/// cluster of several codepoints stored in the pane's [`crate::Tables`].
///
/// Packed into 4 bytes: 0 is empty, a scalar value is itself, and a cluster
/// is its [`GraphemeId`] with the high bit set (scalar values never reach
/// it). The field is private and every constructor (and `Deserialize`)
/// validates, so a `Glyph` always decodes to a [`GlyphKind`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "u32")]
#[repr(transparent)]
pub struct Glyph(u32);

const CLUSTER_BIT: u32 = 1 << 31;

/// Whether `bits` is a glyph: a cluster, or a Unicode scalar value (not a
/// surrogate, at most U+10FFFF). The one rule every scalar check uses;
/// `all_wire` applies it to rows in bulk.
#[inline]
const fn is_glyph(bits: u32) -> bool {
    bits & CLUSTER_BIT != 0 || (bits.wrapping_sub(0xD800) >= 0x800 && bits <= 0x10_FFFF)
}

/// A decoded [`Glyph`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GlyphKind {
    Empty,
    Char(char),
    Cluster(GraphemeId),
}

impl Glyph {
    /// A blank cell's glyph.
    pub const EMPTY: Self = Self(0);

    /// The glyph of a single character. NUL is stored as empty.
    #[must_use]
    #[expect(
        clippy::as_conversions,
        reason = "char to u32 is lossless; u32::from is not const"
    )]
    #[inline]
    pub const fn from_char(c: char) -> Self {
        Self(c as u32)
    }

    /// The glyph of a multi-codepoint cluster.
    #[must_use]
    #[inline]
    pub const fn from_cluster(id: GraphemeId) -> Self {
        Self(id.0 | CLUSTER_BIT)
    }

    #[must_use]
    #[inline]
    pub fn kind(self) -> GlyphKind {
        if self.0 == 0 {
            GlyphKind::Empty
        } else if self.0 & CLUSTER_BIT != 0 {
            GlyphKind::Cluster(GraphemeId(self.0 & !CLUSTER_BIT))
        } else {
            // Only `from_char` and validated deserialisation produce values
            // below the cluster bit.
            GlyphKind::Char(
                char::from_u32(self.0).or_bug("a glyph below the cluster bit is a scalar value"),
            )
        }
    }

    /// The packed value, for encoders.
    #[must_use]
    #[inline]
    pub const fn bits(self) -> u32 {
        self.0
    }
}

/// A `u32` that is neither a Unicode scalar value nor a cluster reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0:#x} is not a valid glyph")]
pub struct InvalidGlyph(pub u32);

impl TryFrom<u32> for Glyph {
    type Error = InvalidGlyph;

    #[inline]
    fn try_from(bits: u32) -> Result<Self, Self::Error> {
        if is_glyph(bits) {
            Ok(Self(bits))
        } else {
            Err(InvalidGlyph(bits))
        }
    }
}

/// Four cells' 8-byte forms (wire or memory, the same bytes) as one vector:
/// [glyph, style+width] × 4, in order.
#[cfg(target_feature = "avx2")]
#[target_feature(enable = "avx2")]
#[inline]
fn four(
    a: [u8; Cell::BYTES],
    b: [u8; Cell::BYTES],
    c: [u8; Cell::BYTES],
    d: [u8; Cell::BYTES],
) -> core::arch::x86_64::__m256i {
    use core::arch::x86_64::_mm256_set_epi64x;
    _mm256_set_epi64x(
        i64::from_le_bytes(d),
        i64::from_le_bytes(c),
        i64::from_le_bytes(b),
        i64::from_le_bytes(a),
    )
}

/// Whether every item of `wire` is a cell's wire form: [`Cell::is_wire`]
/// over the whole row, eight cells per step in AVX2 (plx requires
/// x86-64-v3). Scalar code does not vectorise this: the compiler assembles
/// each 32-bit lane from single bytes (measured, docs/DEVELOPMENT.md
/// "Profiling").
#[cfg(target_feature = "avx2")]
#[target_feature(enable = "avx2")]
#[inline]
fn all_wire(wire: &[[u8; Cell::BYTES]]) -> bool {
    use core::arch::x86_64::{
        _mm256_andnot_si256, _mm256_castps_si256, _mm256_castsi256_ps, _mm256_cmpgt_epi32,
        _mm256_or_si256, _mm256_set1_epi32, _mm256_setzero_si256, _mm256_shuffle_ps,
        _mm256_srai_epi32, _mm256_srli_epi32, _mm256_sub_epi32, _mm256_testz_si256,
        _mm256_xor_si256,
    };
    let (octets, rest) = wire.as_chunks::<8>();
    let sign = _mm256_set1_epi32(i32::MIN);
    let surrogate_base = _mm256_set1_epi32(0xD800);
    let surrogate_span = _mm256_set1_epi32(0x800 ^ i32::MIN);
    let largest_scalar = _mm256_set1_epi32(0x10_FFFF);
    let mut bad = _mm256_setzero_si256();
    for &[a, b, c, d, e, f, g, h] in octets {
        let (low, high) = (
            _mm256_castsi256_ps(four(a, b, c, d)),
            _mm256_castsi256_ps(four(e, f, g, h)),
        );
        // Even lanes are glyphs, odd lanes style+width. The shuffle works
        // within 128-bit halves, so the cells come out reordered, which an
        // all-or-nothing check does not mind.
        let glyphs = _mm256_castps_si256(_mm256_shuffle_ps::<0b10_00_10_00>(low, high));
        let highs = _mm256_castps_si256(_mm256_shuffle_ps::<0b11_01_11_01>(low, high));
        // All ones where the cluster bit is set.
        let cluster = _mm256_srai_epi32::<31>(glyphs);
        // Unsigned `glyph - 0xD800 < 0x800`, through the sign flip that
        // makes AVX2's signed compare unsigned.
        let surrogate = _mm256_cmpgt_epi32(
            surrogate_span,
            _mm256_xor_si256(_mm256_sub_epi32(glyphs, surrogate_base), sign),
        );
        // Signed is right here: a cluster is negative, and excused anyway.
        let too_large = _mm256_cmpgt_epi32(glyphs, largest_scalar);
        let bad_glyph = _mm256_andnot_si256(cluster, _mm256_or_si256(surrogate, too_large));
        // The width is the top byte, at most 3: nothing above bit 25.
        let bad_width = _mm256_srli_epi32::<26>(highs);
        bad = _mm256_or_si256(bad, _mm256_or_si256(bad_glyph, bad_width));
    }
    _mm256_testz_si256(bad, bad) == 1 && rest.iter().all(|&bytes| Cell::is_wire(bytes))
}

/// Whether every item of `wire` is a cell's wire form (portable fallback;
/// x86-64 builds require AVX2, see the crate root).
#[cfg(not(target_feature = "avx2"))]
#[inline]
fn all_wire(wire: &[[u8; Cell::BYTES]]) -> bool {
    wire.iter().all(|&bytes| Cell::is_wire(bytes))
}

/// How many cells at the start of `cells` are plain in `style`
/// ([`Cell::is_plain`]), eight cells per step in AVX2.
#[cfg(target_feature = "avx2")]
#[target_feature(enable = "avx2")]
#[inline]
fn plain_run(cells: &[Cell], style: StyleId) -> usize {
    use core::arch::x86_64::{
        _mm256_and_si256, _mm256_castps_si256, _mm256_castsi256_ps, _mm256_cmpeq_epi32,
        _mm256_cmpgt_epi32, _mm256_movemask_ps, _mm256_or_si256, _mm256_permute4x64_epi64,
        _mm256_set1_epi32, _mm256_setzero_si256, _mm256_shuffle_ps, _mm256_sub_epi32,
        _mm256_xor_si256,
    };
    let (octets, rest) = cells.as_chunks::<8>();
    let sign = _mm256_set1_epi32(i32::MIN);
    let printable_base = _mm256_set1_epi32(0x20);
    let printable_span = _mm256_set1_epi32((0x7F - 0x20) ^ i32::MIN);
    // A plain cell's style+width half is exactly the style id: the width
    // byte above it is Narrow, 0 (asserted next to `Cell`).
    let [s0, s1, s2] = style.0;
    let styled_as = _mm256_set1_epi32(i32::from_le_bytes([s0, s1, s2, 0]));
    let zero = _mm256_setzero_si256();
    let mut run = 0;
    for &[a, b, c, d, e, f, g, h] in octets {
        let (low, high) = (
            _mm256_castsi256_ps(four(a.to_bytes(), b.to_bytes(), c.to_bytes(), d.to_bytes())),
            _mm256_castsi256_ps(four(e.to_bytes(), f.to_bytes(), g.to_bytes(), h.to_bytes())),
        );
        // The shuffle leaves 64-bit units as cells a b e f | c d g h; the
        // permute puts them back in order, so lane n is cell n.
        let glyphs =
            _mm256_permute4x64_epi64::<0b11_01_10_00>(_mm256_castps_si256(_mm256_shuffle_ps::<
                0b10_00_10_00,
            >(low, high)));
        let highs =
            _mm256_permute4x64_epi64::<0b11_01_10_00>(_mm256_castps_si256(_mm256_shuffle_ps::<
                0b11_01_11_01,
            >(low, high)));
        let blank = _mm256_cmpeq_epi32(glyphs, zero);
        // Unsigned `glyph - 0x20 < 0x5F`: printable ASCII.
        let printable = _mm256_cmpgt_epi32(
            printable_span,
            _mm256_xor_si256(_mm256_sub_epi32(glyphs, printable_base), sign),
        );
        let plain = _mm256_and_si256(
            _mm256_cmpeq_epi32(highs, styled_as),
            _mm256_or_si256(blank, printable),
        );
        // One bit per cell, cell 0 lowest.
        let mask = _mm256_movemask_ps(_mm256_castsi256_ps(plain)).cast_unsigned();
        if mask != 0xFF {
            let plain_here = usize::try_from(mask.trailing_ones()).or_bug("at most 8 fits usize");
            return run + plain_here;
        }
        run += 8;
    }
    run + rest.iter().take_while(|cell| cell.is_plain(style)).count()
}

/// How many cells at the start of `cells` are plain in `style` (portable
/// fallback; x86-64 builds require AVX2, see the crate root).
#[cfg(not(target_feature = "avx2"))]
#[inline]
fn plain_run(cells: &[Cell], style: StyleId) -> usize {
    cells.iter().take_while(|cell| cell.is_plain(style)).count()
}

/// A multi-codepoint grapheme cluster in a pane's [`crate::Tables`]
/// (below 2^31).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "u32")]
pub struct GraphemeId(u32);

/// A cluster id past [`GraphemeId::MAX`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0} is past the largest grapheme cluster id")]
pub struct GraphemeIdTooLarge(pub u32);

impl TryFrom<u32> for GraphemeId {
    type Error = GraphemeIdTooLarge;

    #[inline]
    fn try_from(id: u32) -> Result<Self, GraphemeIdTooLarge> {
        Self::new(id).ok_or(GraphemeIdTooLarge(id))
    }
}

impl GraphemeId {
    /// The largest id a glyph can carry.
    pub const MAX: u32 = CLUSTER_BIT - 1;

    /// `None` for ids that do not fit a glyph.
    #[must_use]
    #[inline]
    pub const fn new(id: u32) -> Option<Self> {
        if id <= Self::MAX {
            Some(Self(id))
        } else {
            None
        }
    }

    #[must_use]
    #[inline]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// A style in a pane's [`crate::Tables`]. 0 is [`crate::Style::DEFAULT`].
///
/// 24 bits, packed into 3 bytes so a [`Cell`] stays 8: enough for every cell
/// of the largest grid to have its own style, so a pane's style table can
/// never fill.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(transparent)]
pub struct StyleId([u8; 3]);

impl StyleId {
    pub const DEFAULT: Self = Self([0; 3]);

    /// The largest style id, 2^24 - 1.
    pub const MAX: Self = Self([0xFF; 3]);

    /// `None` past [`StyleId::MAX`].
    #[must_use]
    #[inline]
    pub const fn new(id: u32) -> Option<Self> {
        let [low, mid, high, top] = id.to_le_bytes();
        if top == 0 {
            Some(Self([low, mid, high]))
        } else {
            None
        }
    }

    #[must_use]
    #[inline]
    pub const fn get(self) -> u32 {
        let [low, mid, high] = self.0;
        u32::from_le_bytes([low, mid, high, 0])
    }
}

impl fmt::Debug for StyleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("StyleId").field(&self.get()).finish()
    }
}

impl PartialOrd for StyleId {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for StyleId {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.get().cmp(&other.get())
    }
}

/// How a cell participates in wide-character layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum Width {
    /// An ordinary one-column cell.
    Narrow = 0,
    /// The first column of a two-column character.
    Wide = 1,
    /// The second column of a wide character; it carries no text.
    SpacerTail = 2,
    /// A placeholder at the end of a soft-wrapped row whose wide character
    /// continues on the next row; it carries no text.
    SpacerHead = 3,
}

impl Width {
    const ALL: [Self; 4] = [Self::Narrow, Self::Wide, Self::SpacerTail, Self::SpacerHead];

    /// The width's wire byte: its discriminant, so a cell's memory form is
    /// its wire form ([`Cell::slice_as_wire`]).
    #[must_use]
    #[inline]
    #[expect(
        clippy::as_conversions,
        reason = "a fieldless repr(u8) enum's discriminant; u8::from is not const"
    )]
    pub const fn index(self) -> u8 {
        self as u8
    }

    /// The width with wire byte `index`; `None` for an unknown one.
    #[must_use]
    #[inline]
    pub fn from_index_or_none(index: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|width| width.index() == index)
    }
}

/// One grid cell. Its wire form is [`Cell::to_bytes`]; not serde, so there
/// is one encoding, owned here. Its memory form is the same 8 bytes, so a
/// row is sent without converting it ([`Cell::slice_as_wire`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(C)]
pub struct Cell {
    pub glyph: Glyph,
    pub style: StyleId,
    pub width: Width,
}

// The layout `slice_as_wire` relies on: the wire form's fields at the wire
// form's offsets, 8 bytes with no padding, the glyph little-endian.
const _: () = assert!(size_of::<Cell>() == 8, "a Cell must stay 8 bytes");
const _: () = assert!(core::mem::offset_of!(Cell, glyph) == 0 && size_of::<Glyph>() == 4);
const _: () = assert!(core::mem::offset_of!(Cell, style) == 4 && size_of::<StyleId>() == 3);
const _: () = assert!(core::mem::offset_of!(Cell, width) == 7 && size_of::<Width>() == 1);
#[cfg(not(target_endian = "little"))]
compile_error!("a Cell's memory form is its wire form, which is little-endian");
// `Cell::is_wire` (`< 4 << 24`) and `all_wire` (nothing above bit 25) check
// the width as a two-bit value in the top byte.
const _: () = assert!(
    Width::SpacerHead.index() == 3,
    "the wire checks assume widths 0 to 3"
);
// `plain_run` compares a cell's style+width half with the bare style id.
const _: () = assert!(Width::Narrow.index() == 0, "a plain cell's width byte is 0");

/// Eight bytes that are not a cell: a glyph that is neither a scalar value
/// nor a cluster, or an unknown width.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0:02x?} is not a cell")]
pub struct InvalidCell(pub [u8; Cell::BYTES]);

impl Cell {
    /// An empty, default-styled, narrow cell.
    pub const BLANK: Self = Self {
        glyph: Glyph::EMPTY,
        style: StyleId::DEFAULT,
        width: Width::Narrow,
    };

    /// The size of a cell's wire form.
    pub const BYTES: usize = 8;

    /// The wire form: the glyph (4 bytes, little-endian), the style id (3
    /// bytes, little-endian), the width (1 byte).
    #[must_use]
    #[inline]
    pub const fn to_bytes(self) -> [u8; Self::BYTES] {
        let [g0, g1, g2, g3] = self.glyph.bits().to_le_bytes();
        let [s0, s1, s2, _] = self.style.get().to_le_bytes();
        [g0, g1, g2, g3, s0, s1, s2, self.width.index()]
    }

    /// The wire forms of `cells`, borrowed: each cell's 8 bytes in memory
    /// are its [`Cell::to_bytes`]. A row is sent as one block, uncopied.
    #[must_use]
    #[inline]
    #[expect(
        unsafe_code,
        reason = "viewing cells as their bytes; layout proven by the const asserts above"
    )]
    pub const fn slice_as_wire(cells: &[Self]) -> &[u8] {
        // SAFETY: `Cell` is `repr(C)`: a `repr(transparent)` u32 at offset
        // 0, a `repr(transparent)` [u8; 3] at 4 and a `repr(u8)` enum at 7,
        // 8 bytes in all (asserted above), so it has no padding and every
        // byte of `cells` is initialised. `u8` has alignment 1 and no
        // invalid values, so those bytes may be read as `u8`s. The length is
        // exactly the slice's extent in bytes, and the result borrows
        // `cells` immutably for its lifetime, so nothing can write through
        // or free it meanwhile.
        unsafe {
            core::slice::from_raw_parts(cells.as_ptr().cast::<u8>(), cells.len() * Self::BYTES)
        }
    }

    /// Whether `bytes` is a cell's wire form: the glyph is a glyph and the
    /// width a width (every style id is valid). The one rule
    /// [`Cell::from_bytes`] and [`Cell::copy_from_wire`] check; `all_wire`
    /// is the same rule four cells at a time (a property test holds them
    /// equal).
    #[inline]
    const fn is_wire(bytes: [u8; Self::BYTES]) -> bool {
        // In 32-bit halves, as `all_wire` reads them: the glyph, then the
        // style with the width in the top byte, which is at most 3.
        let [g0, g1, g2, g3, s0, s1, s2, width] = bytes;
        is_glyph(u32::from_le_bytes([g0, g1, g2, g3]))
            && u32::from_le_bytes([s0, s1, s2, width]) < 4 << 24
    }

    /// A cell from its wire form.
    ///
    /// # Errors
    ///
    /// When the glyph or width is invalid.
    #[inline]
    #[track_caller]
    pub fn from_bytes(bytes: [u8; Self::BYTES]) -> Result<Self, InvalidCell> {
        if !Self::is_wire(bytes) {
            return Err(InvalidCell(bytes));
        }
        let [g0, g1, g2, g3, s0, s1, s2, width] = bytes;
        Ok(Self {
            glyph: Glyph(u32::from_le_bytes([g0, g1, g2, g3])),
            style: StyleId([s0, s1, s2]),
            width: Width::from_index_or_none(width).or_bug("is_wire checked the width"),
        })
    }

    /// Overwrites `cells` with the cells whose wire forms are `wire`, which
    /// must be as many: every one is checked first, then all are copied in
    /// one go. A decoded row goes into its grid row without a per-cell
    /// conversion.
    ///
    /// # Errors
    ///
    /// The first wire form that is not a cell; `cells` is then unchanged.
    #[inline]
    #[track_caller]
    #[expect(
        unsafe_code,
        reason = "copying checked wire forms into cells' memory; layout proven by the const asserts above"
    )]
    pub fn copy_from_wire(
        cells: &mut [Self],
        wire: &[[u8; Self::BYTES]],
    ) -> Result<(), InvalidCell> {
        plx_bug::invariant!(
            cells.len() == wire.len(),
            "wire forms are copied into exactly as many cells"
        );
        #[cfg(target_feature = "avx2")]
        // SAFETY: `all_wire` only requires AVX2, which this build enables
        // (the cfg above).
        let valid = unsafe { all_wire(wire) };
        #[cfg(not(target_feature = "avx2"))]
        let valid = all_wire(wire);
        if !valid {
            let first = wire
                .iter()
                .copied()
                .find(|&bytes| !Self::is_wire(bytes))
                .or_bug("a row that failed the check holds an invalid wire form");
            return Err(InvalidCell(first));
        }
        // SAFETY: `wire` and `cells` hold equally many 8-byte items
        // (checked above), so source and destination are both
        // `size_of_val(wire)` bytes; they cannot overlap, one being borrowed
        // shared and the other exclusively, and a byte copy needs no
        // alignment. No reference into `cells` is live during the copy.
        // Afterwards every cell's bytes are a wire form `is_wire` accepted,
        // and a cell's memory form is its wire form (`repr(C)`, the offsets
        // and size asserted above, little-endian): the glyph is a u32 (no
        // invalid bit patterns) that is a scalar value or a cluster, as
        // `Glyph` promises; the style is any 3 bytes, as `StyleId` allows;
        // the width byte is at most 3, a declared discriminant of the
        // `repr(u8)` `Width`. So every cell is a valid `Cell`, equal to
        // `Cell::from_bytes` of its bytes.
        unsafe {
            core::ptr::copy_nonoverlapping(
                wire.as_ptr().cast::<u8>(),
                cells.as_mut_ptr().cast::<u8>(),
                size_of_val(wire),
            );
        }
        Ok(())
    }

    /// Whether this cell is plain in `style`: narrow, printable ASCII or
    /// blank, in that style. A terminal shows it as one byte
    /// ([`Cell::plain_byte`]) and measures it as one column.
    #[must_use]
    #[inline]
    pub const fn is_plain(self, style: StyleId) -> bool {
        let glyph = self.glyph.0;
        matches!(self.width, Width::Narrow)
            && self.style.get() == style.get()
            && (glyph == 0 || (glyph >= 0x20 && glyph < 0x7F))
    }

    /// How many cells at the start of `cells` are plain in `style`
    /// ([`Cell::is_plain`]): a run the compositor writes as one byte each.
    #[must_use]
    #[inline]
    #[expect(
        unsafe_code,
        reason = "calling the AVX2 scan, which needs only what this build enables"
    )]
    pub fn plain_run(cells: &[Self], style: StyleId) -> usize {
        #[cfg(target_feature = "avx2")]
        // SAFETY: `plain_run` only requires AVX2, which this build enables
        // (the cfg above).
        let run = unsafe { plain_run(cells, style) };
        #[cfg(not(target_feature = "avx2"))]
        let run = plain_run(cells, style);
        run
    }

    /// The byte a plain cell ([`Cell::is_plain`]) is written as: a
    /// printable ASCII glyph is its own low byte, and a blank (0) becomes a
    /// space, the smallest printable byte. Meaningless for other cells.
    #[must_use]
    #[inline]
    pub const fn plain_byte(self) -> u8 {
        let [low, ..] = self.glyph.0.to_le_bytes();
        if low < b' ' { b' ' } else { low }
    }
}

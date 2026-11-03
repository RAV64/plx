//! [`GhosttyEmulator`]: one libghostty-vt terminal, the screen it
//! syncs into, and the plumbing that turns its callbacks into
//! [`EmulatorEvents`].

use core::ffi::c_void;
use core::fmt;
use core::num::NonZeroUsize;
use core::ptr::{self, NonNull};

use plx_bug::{OrBug as _, breach, contract};
use plx_emulator::{
    ActiveScreen, Autoscroll, CellPx, Change, Compression, Damage, Emulator, EmulatorEvents,
    EmulatorFactory, GridSize, KeyEvent, MAX_CLIPBOARD_BYTES, MouseEvent, MouseTracking, Percent,
    Progress, Screen, Scroll, ScrollPosition, SearchDirection, SearchOptions, SearchStatus,
    SelectAt, SelectGesture, Selection, Version,
};
use plx_ghostty_sys as sys;

use crate::build::Feature;
use crate::error::{Error, adopt, check, check_contract, header_u16};
use crate::input::{self, KeyEncoder, MouseEncoder, PasteMode};
use crate::search::{Recount, Searcher, View};
use crate::selection::{self, Selector, Surface};
use crate::sync::{GridSync, SyncScope};

/// How much scrollback history a terminal keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scrollback {
    /// No history: lines scrolled off the top are dropped.
    Disabled,
    /// At most this many bytes of history (pruned in whole pages).
    Bytes(NonZeroUsize),
    /// At least this many lines of history, however wide; more are kept up
    /// to the next whole page (dozens to a hundred lines).
    Lines(NonZeroUsize),
    /// No limit.
    Unlimited,
}

/// How to configure a terminal.
///
/// Every setting is explicit; there are no hidden defaults. The same settings
/// apply to new and restored terminals, because a snapshot carries terminal
/// state but not configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Options {
    pub scrollback: Scrollback,
    /// The longest unfinished escape sequence kept so that a snapshot taken
    /// mid-sequence can be restored and resumed exactly. libghostty-vt's own
    /// default is to keep none, which makes such snapshots fail.
    pub continuation_max_bytes: NonZeroUsize,
    pub grapheme_clustering: GraphemeClustering,
}

/// Whether grapheme clustering (DEC mode 2027) is on after a reset. Programs
/// can still toggle the mode themselves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphemeClustering {
    /// Emoji ZWJ sequences, flags and combining marks occupy one cell, with
    /// Unicode-aware widths.
    Enabled,
    /// Legacy per-codepoint widths.
    Disabled,
}

impl GraphemeClustering {
    const fn mode_state(self) -> ModeState {
        match self {
            Self::Enabled => ModeState::Set,
            Self::Disabled => ModeState::Reset,
        }
    }
}

/// Whether a terminal mode is on (set) or off (reset).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ModeState {
    Set,
    Reset,
}

/// From the C API's mode value.
impl From<bool> for ModeState {
    fn from(set: bool) -> Self {
        if set { Self::Set } else { Self::Reset }
    }
}

impl ModeState {
    /// As the C API's mode value.
    const fn bit(self) -> bool {
        match self {
            Self::Set => true,
            Self::Reset => false,
        }
    }
}

/// DEC private mode 2027 as a packed `GhosttyMode` (bit 15 clear = DEC
/// private). The header builds it with a `static inline` function that
/// bindgen cannot bind (`GHOSTTY_MODE_GRAPHEME_CLUSTER`).
const MODE_GRAPHEME_CLUSTER: sys::GhosttyMode = 2027;
/// DEC private mode 2004, bracketed paste.
const MODE_BRACKETED_PASTE: sys::GhosttyMode = 2004;

/// A terminal emulator backed by libghostty-vt.
pub struct GhosttyEmulator {
    // Field order is drop order: the terminal goes first, so no callback can
    // run after the sink it writes into has been freed.
    terminal: TerminalHandle,
    sync: GridSync,
    keys: KeyEncoder,
    mouse: MouseEncoder,
    selector: Selector,
    searcher: Searcher,
    /// The cell size the client reported, for pixel mouse reports.
    cell_px: Option<CellPx>,
    /// Reused input buffer for the paste encoder, which rewrites in place.
    paste_scratch: Vec<u8>,
    /// The visible screen and what it refers to, as of the last sync. Owned
    /// here, so it can only ever be synced from this terminal.
    screen: Screen,
    /// What the next sync must read: everything after creation, restore or
    /// resize, otherwise libghostty's dirty rows.
    next_sync: SyncScope,
    sink: SinkBox,
}

impl GhosttyEmulator {
    /// Creates a blank terminal.
    ///
    /// # Errors
    ///
    /// When libghostty-vt cannot create the terminal or its helpers.
    pub fn new(size: GridSize, options: &Options) -> Result<Self, Error> {
        const CALL: &str = "ghostty_terminal_new";
        let mut raw: sys::GhosttyTerminal = ptr::null_mut();
        // SAFETY: valid out-pointer and allocator.
        check(CALL, unsafe {
            sys::ghostty_terminal_new(
                crate::alloc::rust(),
                &raw mut raw,
                size.cols().get(),
                size.rows().get(),
            )
        })?;
        let emulator = Self::wire(TerminalHandle(adopt(raw, CALL)), size)?;
        emulator.set_scrollback(options.scrollback);
        emulator.set_usize(
            "ghostty_terminal_set(CONTINUATION_MAX_BYTES)",
            sys::GhosttyTerminalOption::GHOSTTY_TERMINAL_OPT_CONTINUATION_MAX_BYTES,
            options.continuation_max_bytes.get(),
        );
        emulator.set_mode(
            "ghostty_terminal_set(MODE_DEFAULT)",
            sys::GhosttyTerminalOption::GHOSTTY_TERMINAL_OPT_MODE_DEFAULT,
            MODE_GRAPHEME_CLUSTER,
            options.grapheme_clustering.mode_state(),
        );
        Ok(emulator)
    }

    /// Recreates a terminal from [`Emulator::snapshot`] output of the same
    /// libghostty-vt build. The grid size and all terminal state come from
    /// the snapshot; callbacks and configuration are not part of a snapshot,
    /// so they come from `options`.
    ///
    /// # Errors
    ///
    /// When the snapshot is malformed, comes from an incompatible build, or
    /// holds an unfinished sequence longer than
    /// `options.continuation_max_bytes`.
    pub fn restore(snapshot: &[u8], options: &Options) -> Result<Self, Error> {
        let decoder = Decoder::new(snapshot)?;
        let max_continuation: usize = options.continuation_max_bytes.get();
        decoder.set(
            "ghostty_snapshot_decoder_set(MAX_CONTINUATION_BYTES)",
            sys::GhosttySnapshotDecoderOption::GHOSTTY_SNAPSHOT_DECODER_OPT_MAX_CONTINUATION_BYTES,
            (&raw const max_continuation).cast(),
        )?;
        // Keep tracking the restored unfinished sequence, so that this
        // terminal can itself be snapshotted mid-sequence later.
        let retain = true;
        decoder.set(
            "ghostty_snapshot_decoder_set(RETAIN_CONTINUATION)",
            sys::GhosttySnapshotDecoderOption::GHOSTTY_SNAPSHOT_DECODER_OPT_RETAIN_CONTINUATION,
            (&raw const retain).cast(),
        )?;
        let terminal = decoder.decode()?;

        // The snapshot is untrusted: its size is parsed like a client's.
        let size = GridSize::new(
            terminal
                .get_u16(sys::GhosttyTerminalData::GHOSTTY_TERMINAL_DATA_COLS)
                .try_into()
                .or_contract("a terminal has columns"),
            terminal
                .get_u16(sys::GhosttyTerminalData::GHOSTTY_TERMINAL_DATA_ROWS)
                .try_into()
                .or_contract("a terminal has rows"),
        )?;
        let emulator = Self::wire(terminal, size)?;
        emulator.set_scrollback(options.scrollback);
        // Setting a reset default also overwrites the current value, which the
        // snapshot restored (the program may have toggled the mode), so put the
        // restored value back afterwards.
        let current = emulator.terminal.get_mode(MODE_GRAPHEME_CLUSTER);
        emulator.set_mode(
            "ghostty_terminal_set(MODE_DEFAULT)",
            sys::GhosttyTerminalOption::GHOSTTY_TERMINAL_OPT_MODE_DEFAULT,
            MODE_GRAPHEME_CLUSTER,
            options.grapheme_clustering.mode_state(),
        );
        emulator.set_mode(
            "ghostty_terminal_set(MODE)",
            sys::GhosttyTerminalOption::GHOSTTY_TERMINAL_OPT_MODE,
            MODE_GRAPHEME_CLUSTER,
            current,
        );
        Ok(emulator)
    }

    /// Rows of history above the screen: what the scrollback holds now.
    #[must_use]
    pub fn scrollback_rows(&self) -> usize {
        self.terminal
            .get_usize(sys::GhosttyTerminalData::GHOSTTY_TERMINAL_DATA_SCROLLBACK_ROWS)
    }

    /// Installs the sink and callbacks on a fresh terminal.
    ///
    /// # Errors
    ///
    /// When libghostty-vt cannot create the render state or key encoder.
    fn wire(terminal: TerminalHandle, size: GridSize) -> Result<Self, Error> {
        let emulator = Self {
            terminal,
            sync: GridSync::new()?,
            keys: KeyEncoder::new()?,
            mouse: MouseEncoder::new()?,
            selector: Selector::new()?,
            searcher: Searcher::new(),
            cell_px: None,
            paste_scratch: Vec::new(),
            screen: Screen::new(size),
            next_sync: SyncScope::Full,
            sink: SinkBox::new(),
        };
        emulator.set_ptr(
            "ghostty_terminal_set(USERDATA)",
            sys::GhosttyTerminalOption::GHOSTTY_TERMINAL_OPT_USERDATA,
            emulator.sink.as_userdata(),
        );
        emulator.install_callbacks();
        emulator.turn_off_unreportable_features();
        emulator.mark_the_default_cursor();
        Ok(emulator)
    }

    /// Compresses every page of history that can be, in one go: it can
    /// stall on a long history (benchmarks and tests; the session steps
    /// with [`Emulator::compress`]).
    pub fn compress_fully(&mut self) -> Compression {
        self.compress_with(
            sys::GhosttyTerminalCompressionMode::GHOSTTY_TERMINAL_COMPRESSION_MODE_FULL,
        )
    }

    fn compress_with(&mut self, mode: sys::GhosttyTerminalCompressionMode) -> Compression {
        use sys::GhosttyTerminalCompressionResult as Outcome;
        let mut outcome = Outcome::GHOSTTY_TERMINAL_COMPRESSION_RESULT_COMPLETE;
        // SAFETY: the terminal is live and nothing else uses it during the
        // call; the mode is one the header lists; `outcome` is a valid
        // out-pointer.
        check_contract("ghostty_terminal_compress", unsafe {
            sys::ghostty_terminal_compress(self.terminal.as_ptr(), mode, &raw mut outcome)
        });
        match outcome {
            Outcome::GHOSTTY_TERMINAL_COMPRESSION_RESULT_PENDING => Compression::Pending,
            // Unsupported here: nothing to do, ever.
            Outcome::GHOSTTY_TERMINAL_COMPRESSION_RESULT_COMPLETE
            | Outcome::GHOSTTY_TERMINAL_COMPRESSION_RESULT_UNSUPPORTED => Compression::Done,
            other => breach!("the header lists every compression result, not {}", other.0),
        }
    }

    /// The memory the terminal's pages hold, both screens together.
    #[must_use]
    pub fn memory_usage(&self) -> MemoryUsage {
        let mut usage = sys::GhosttyTerminalMemoryUsage {
            size: size_of::<sys::GhosttyTerminalMemoryUsage>(),
            ..sys::GhosttyTerminalMemoryUsage::default()
        };
        // SAFETY: the terminal is live; `DATA_MEMORY_USAGE` documents a
        // sized `GhosttyTerminalMemoryUsage` output.
        check_contract("ghostty_terminal_get(MEMORY_USAGE)", unsafe {
            sys::ghostty_terminal_get(
                self.terminal.as_ptr(),
                sys::GhosttyTerminalData::GHOSTTY_TERMINAL_DATA_MEMORY_USAGE,
                (&raw mut usage).cast(),
            )
        });
        MemoryUsage {
            resident_bytes: usage.primary_resident_bytes + usage.alternate_resident_bytes,
            reserved_bytes: usage.primary_virtual_bytes + usage.alternate_virtual_bytes,
            pages: usage.primary_pages + usage.alternate_pages,
            compressed_pages: usage.primary_compressed_pages + usage.alternate_compressed_pages,
        }
    }

    /// The terminal's size, as the view a search marks matches in.
    fn view(&self) -> View {
        View {
            cols: self
                .terminal
                .get_u16(sys::GhosttyTerminalData::GHOSTTY_TERMINAL_DATA_COLS),
            rows: self
                .terminal
                .get_u16(sys::GhosttyTerminalData::GHOSTTY_TERMINAL_DATA_ROWS),
        }
    }

    /// The terminal as a selection gesture measures it.
    fn surface(&self) -> Surface {
        let View { cols, rows } = self.view();
        Surface {
            cols,
            rows,
            cell_px: self.cell_px,
        }
    }

    #[expect(
        clippy::as_conversions,
        reason = "the C API takes callbacks as `const void *`"
    )]
    fn install_callbacks(&self) {
        // Typing each callback as bindgen's alias makes the compiler check its
        // signature against the header.
        const _: sys::GhosttyTerminalWritePtyFn = Some(on_write_pty);
        const _: sys::GhosttyTerminalBellFn = Some(on_bell);
        const _: sys::GhosttyTerminalDeviceAttributesFn = Some(on_device_attributes);
        const _: sys::GhosttyTerminalClipboardWriteFn = Some(on_clipboard_write);
        const _: sys::GhosttyTerminalProgressReportFn = Some(on_progress_report);
        self.set_ptr(
            "ghostty_terminal_set(WRITE_PTY)",
            sys::GhosttyTerminalOption::GHOSTTY_TERMINAL_OPT_WRITE_PTY,
            on_write_pty as extern "C" fn(_, _, _, _) as *const c_void,
        );
        self.set_ptr(
            "ghostty_terminal_set(BELL)",
            sys::GhosttyTerminalOption::GHOSTTY_TERMINAL_OPT_BELL,
            on_bell as extern "C" fn(_, _) as *const c_void,
        );
        self.set_ptr(
            "ghostty_terminal_set(DEVICE_ATTRIBUTES)",
            sys::GhosttyTerminalOption::GHOSTTY_TERMINAL_OPT_DEVICE_ATTRIBUTES,
            on_device_attributes as extern "C" fn(_, _, _) -> _ as *const c_void,
        );
        self.set_ptr(
            "ghostty_terminal_set(CLIPBOARD_WRITE)",
            sys::GhosttyTerminalOption::GHOSTTY_TERMINAL_OPT_CLIPBOARD_WRITE,
            on_clipboard_write as unsafe extern "C" fn(_, _, _) as *const c_void,
        );
        self.set_ptr(
            "ghostty_terminal_set(PROGRESS_REPORT)",
            sys::GhosttyTerminalOption::GHOSTTY_TERMINAL_OPT_PROGRESS_REPORT,
            on_progress_report as unsafe extern "C" fn(_, _, _) as *const c_void,
        );
    }

    fn set_scrollback(&self, scrollback: Scrollback) {
        const BYTES_CALL: &str = "ghostty_terminal_set(SCROLLBACK_MAX_BYTES)";
        const BYTES: sys::GhosttyTerminalOption =
            sys::GhosttyTerminalOption::GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_BYTES;
        const LINES_CALL: &str = "ghostty_terminal_set(SCROLLBACK_MAX_LINES)";
        const LINES: sys::GhosttyTerminalOption =
            sys::GhosttyTerminalOption::GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_LINES;
        // Each limit applies when set; a null value removes it.
        let (bytes, lines) = match scrollback {
            Scrollback::Disabled => (Some(0), None),
            Scrollback::Bytes(bytes) => (Some(bytes.get()), None),
            Scrollback::Lines(lines) => (None, Some(lines.get())),
            Scrollback::Unlimited => (None, None),
        };
        match bytes {
            Some(bytes) => self.set_usize(BYTES_CALL, BYTES, bytes),
            None => self.set_ptr(BYTES_CALL, BYTES, ptr::null()),
        }
        match lines {
            Some(lines) => self.set_usize(LINES_CALL, LINES, lines),
            None => self.set_ptr(LINES_CALL, LINES, ptr::null()),
        }
    }

    /// Turns off each compiled-out feature that build info cannot report, so
    /// it stays off even if a library built with it is ever linked.
    /// Makes the cursor style a program gets when it asks for none (or
    /// resets it, `CSI 0 q`) a hollow block, which no escape sequence
    /// selects: the sync reports it as [`CursorShape::Default`], and the
    /// client's terminal draws its user's own cursor.
    ///
    /// [`CursorShape::Default`]: plx_emulator::CursorShape::Default
    fn mark_the_default_cursor(&self) {
        let hollow = sys::GhosttyTerminalCursorStyle::GHOSTTY_TERMINAL_CURSOR_STYLE_BLOCK_HOLLOW;
        self.set_ptr(
            "ghostty_terminal_set(DEFAULT_CURSOR_STYLE)",
            sys::GhosttyTerminalOption::GHOSTTY_TERMINAL_OPT_DEFAULT_CURSOR_STYLE,
            (&raw const hollow).cast(),
        );
    }

    fn turn_off_unreportable_features(&self) {
        let off = false;
        for feature in Feature::COMPILED_OUT {
            match feature {
                // Reported by build info and checked by `verify_build`.
                Feature::KittyGraphics => {}
                Feature::GlyphProtocol => self.set_ptr(
                    "ghostty_terminal_set(GLYPH_PROTOCOL)",
                    sys::GhosttyTerminalOption::GHOSTTY_TERMINAL_OPT_GLYPH_PROTOCOL,
                    (&raw const off).cast(),
                ),
            }
        }
    }

    fn set_mode(
        &self,
        call: &'static str,
        option: sys::GhosttyTerminalOption,
        mode: sys::GhosttyMode,
        state: ModeState,
    ) {
        let config = sys::GhosttyTerminalModeConfig {
            mode,
            value: state.bit(),
        };
        self.set_ptr(call, option, (&raw const config).cast());
    }

    fn set_usize(&self, call: &'static str, option: sys::GhosttyTerminalOption, value: usize) {
        self.set_ptr(call, option, (&raw const value).cast());
    }

    fn set_ptr(
        &self,
        call: &'static str,
        option: sys::GhosttyTerminalOption,
        value: *const c_void,
    ) {
        // SAFETY: the terminal is live and no write is in progress; every
        // caller passes a value of the type documented for `option`, which the
        // C API reads during the call only (callbacks and userdata are stored
        // by value; userdata outlives the terminal, see field order). The
        // options plx sets are constant and valid, and the header documents
        // no failure but invalid arguments.
        check_contract(call, unsafe {
            sys::ghostty_terminal_set(self.terminal.as_ptr(), option, value)
        });
    }
}

impl Emulator for GhosttyEmulator {
    type Error = Error;

    fn feed(&mut self, bytes: &[u8]) -> &EmulatorEvents {
        self.sink.get_mut().events.clear();
        // SAFETY: the terminal is live; `bytes` is valid for its length. The
        // callbacks invoked during the call touch only the sink, which nothing
        // else accesses while `&mut self` is held.
        unsafe {
            sys::ghostty_terminal_vt_write(self.terminal.as_ptr(), bytes.as_ptr(), bytes.len());
        }
        &self.sink.get_mut().events
    }

    fn resize(
        &mut self,
        size: GridSize,
        cell_px: Option<CellPx>,
    ) -> Result<&EmulatorEvents, Error> {
        self.sink.get_mut().events.clear();
        // libghostty-vt takes zero as "pixel size unknown" for size reports.
        let (width_px, height_px) = cell_px.map_or((0, 0), |px| (px.width.get(), px.height.get()));
        // SAFETY: the terminal is live; callbacks touch only the sink.
        let result = unsafe {
            sys::ghostty_terminal_resize(
                self.terminal.as_ptr(),
                size.cols().get(),
                size.rows().get(),
                width_px,
                height_px,
            )
        };
        check("ghostty_terminal_resize", result)?;
        self.cell_px = cell_px;
        self.next_sync = SyncScope::Full;
        Ok(&self.sink.get_mut().events)
    }

    fn sync(&mut self, version: Version) -> Result<Damage, Error> {
        // New output can add, move or drop matches; changed marks change
        // rows libghostty does not report dirty.
        let view = self.view();
        if self
            .searcher
            .refresh(self.terminal.as_ptr(), view, Recount::IfCheap)
            == Change::Changed
        {
            self.next_sync = SyncScope::Full;
        }
        let damage = self.sync.sync(
            self.terminal.0,
            &mut self.screen,
            self.next_sync,
            version,
            self.searcher.marks(),
        )?;
        self.next_sync = SyncScope::Dirty;
        Ok(damage)
    }

    fn screen(&self) -> &Screen {
        &self.screen
    }

    fn snapshot(&mut self) -> Result<Vec<u8>, Error> {
        const CALL: &str = "ghostty_snapshot_encode_alloc";
        let mut data: *mut u8 = ptr::null_mut();
        let mut len: usize = 0;
        // SAFETY: the terminal is live; valid out-pointers and allocator. It
        // can fail: a pane can leave an unfinished sequence longer than
        // `continuation_max_bytes`, which no snapshot can carry.
        check(CALL, unsafe {
            sys::ghostty_snapshot_encode_alloc(
                self.terminal.as_ptr(),
                crate::alloc::rust(),
                &raw mut data,
                &raw mut len,
            )
        })?;
        let data = adopt(data, CALL);
        // SAFETY: libghostty-vt allocated `len` initialised bytes at `data`.
        let encoded = unsafe { core::slice::from_raw_parts(data.as_ptr(), len) };
        #[expect(
            clippy::disallowed_methods,
            reason = "libghostty owns the encoded buffer and frees it right below; \
                      one copy into a Rust-owned Vec, only on attach or upgrade"
        )]
        let bytes = encoded.to_vec();
        // SAFETY: `data`/`len` came from this allocator above and are freed
        // exactly once, with the same allocator.
        unsafe { sys::ghostty_free(crate::alloc::rust(), data.as_ptr(), len) };
        Ok(bytes)
    }

    fn encode_key(&mut self, key: &KeyEvent, out: &mut Vec<u8>) {
        self.keys.encode(self.terminal.as_ptr(), key, out);
    }

    fn encode_paste(&mut self, text: &str, out: &mut Vec<u8>) {
        let mode = match self.terminal.get_mode(MODE_BRACKETED_PASTE) {
            ModeState::Set => PasteMode::Bracketed,
            ModeState::Reset => PasteMode::Plain,
        };
        input::encode_paste(text, mode, &mut self.paste_scratch, out);
    }

    fn scroll(&mut self, scroll: Scroll) {
        use sys::GhosttyTerminalScrollViewportTag as Tag;
        let rows = |rows: u16| isize::try_from(rows).or_bug("a u16 fits isize on 64-bit targets");
        let (tag, delta) = match scroll {
            Scroll::Up(n) => (Tag::GHOSTTY_SCROLL_VIEWPORT_DELTA, -rows(n)),
            Scroll::Down(n) => (Tag::GHOSTTY_SCROLL_VIEWPORT_DELTA, rows(n)),
            Scroll::Top => (Tag::GHOSTTY_SCROLL_VIEWPORT_TOP, 0),
            Scroll::Bottom => (Tag::GHOSTTY_SCROLL_VIEWPORT_BOTTOM, 0),
            Scroll::Prompts(n) => (Tag::GHOSTTY_SCROLL_VIEWPORT_DELTA_PROMPT, isize::from(n)),
        };
        // `delta` and `delta_prompt` share the union's first word.
        let value = match scroll {
            Scroll::Prompts(_) => sys::GhosttyTerminalScrollViewportValue {
                delta_prompt: delta,
            },
            Scroll::Up(_) | Scroll::Down(_) | Scroll::Top | Scroll::Bottom => {
                sys::GhosttyTerminalScrollViewportValue { delta }
            }
        };
        let behavior = sys::GhosttyTerminalScrollViewport { tag, value };
        // SAFETY: the terminal is live; the field written is the one the tag
        // reads (`delta` for DELTA, `delta_prompt` for DELTA_PROMPT), and the
        // other tags read none.
        unsafe { sys::ghostty_terminal_scroll_viewport(self.terminal.as_ptr(), behavior) };
        // The whole view changed.
        self.next_sync = SyncScope::Full;
    }

    fn mouse_tracking(&self) -> MouseTracking {
        let tracking = self
            .terminal
            .get_bool(sys::GhosttyTerminalData::GHOSTTY_TERMINAL_DATA_MOUSE_TRACKING);
        if tracking {
            MouseTracking::On
        } else {
            MouseTracking::Off
        }
    }

    fn active_screen(&self) -> ActiveScreen {
        let mut screen = sys::GhosttyTerminalScreen::GHOSTTY_TERMINAL_SCREEN_PRIMARY;
        // SAFETY: the terminal is live; `DATA_ACTIVE_SCREEN` documents a
        // `GhosttyTerminalScreen` output.
        check_contract("ghostty_terminal_get(ACTIVE_SCREEN)", unsafe {
            sys::ghostty_terminal_get(
                self.terminal.as_ptr(),
                sys::GhosttyTerminalData::GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN,
                (&raw mut screen).cast(),
            )
        });
        if screen == sys::GhosttyTerminalScreen::GHOSTTY_TERMINAL_SCREEN_ALTERNATE {
            ActiveScreen::Alternate
        } else {
            ActiveScreen::Primary
        }
    }

    fn encode_mouse(&mut self, mouse: &MouseEvent, out: &mut Vec<u8>) {
        let cols = self
            .terminal
            .get_u16(sys::GhosttyTerminalData::GHOSTTY_TERMINAL_DATA_COLS);
        let rows = self
            .terminal
            .get_u16(sys::GhosttyTerminalData::GHOSTTY_TERMINAL_DATA_ROWS);
        self.mouse.encode(
            self.terminal.as_ptr(),
            (cols, rows),
            self.cell_px,
            mouse,
            out,
        );
    }

    fn select(&mut self, gesture: SelectGesture, at: SelectAt, time_ns: u64) -> Autoscroll {
        let surface = self.surface();
        let autoscroll =
            self.selector
                .gesture(self.terminal.as_ptr(), gesture, at, time_ns, surface);
        // The selection's cells change look.
        self.next_sync = SyncScope::Full;
        autoscroll
    }

    fn autoscroll_selection(&mut self) -> Autoscroll {
        let surface = self.surface();
        let autoscroll = self
            .selector
            .autoscroll_tick(self.terminal.as_ptr(), surface);
        // The view moved, and the selection with it.
        self.next_sync = SyncScope::Full;
        autoscroll
    }

    fn selection_text(&mut self, out: &mut Vec<u8>) -> Selection {
        selection::text(self.terminal.as_ptr(), out)
    }

    fn clear_selection(&mut self) {
        selection::clear(self.terminal.as_ptr());
        self.next_sync = SyncScope::Full;
    }

    fn search(&mut self, needle: &str, options: SearchOptions) {
        let view = self.view();
        self.searcher
            .set(self.terminal.as_ptr(), needle, options, view);
        // The matches change look.
        self.next_sync = SyncScope::Full;
    }

    fn search_select(&mut self, direction: SearchDirection) {
        let view = self.view();
        self.searcher
            .select(self.terminal.as_ptr(), direction, view);
        // The view may have moved, and the selected match changes look.
        self.next_sync = SyncScope::Full;
    }

    fn search_status(&self) -> SearchStatus {
        self.searcher.status()
    }

    fn history_text(&mut self, out: &mut Vec<u8>) {
        selection::all_text(self.terminal.as_ptr(), out);
    }

    fn compress(&mut self) -> Compression {
        self.compress_with(
            sys::GhosttyTerminalCompressionMode::GHOSTTY_TERMINAL_COMPRESSION_MODE_INCREMENTAL,
        )
    }

    fn scroll_position(&self) -> ScrollPosition {
        let mut bar = sys::GhosttyTerminalScrollbar::default();
        // SAFETY: the terminal is live; `DATA_SCROLLBAR` documents a
        // `GhosttyTerminalScrollbar` output.
        check_contract("ghostty_terminal_get(SCROLLBAR)", unsafe {
            sys::ghostty_terminal_get(
                self.terminal.as_ptr(),
                sys::GhosttyTerminalData::GHOSTTY_TERMINAL_DATA_SCROLLBAR,
                (&raw mut bar).cast(),
            )
        });
        // The scrollable area is the history plus the screen; the offset is
        // the view's top row in it.
        let history = bar.total.saturating_sub(bar.len);
        ScrollPosition {
            back: history.saturating_sub(bar.offset),
            history,
        }
    }
}

/// The memory a terminal's pages hold (both screens).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryUsage {
    /// Physical memory in use: a compressed page counts its compressed
    /// size. The figure to budget against.
    pub resident_bytes: u64,
    /// Address space reserved: compressing does not lower it, as each page
    /// keeps its room to be restored.
    pub reserved_bytes: u64,
    pub pages: u64,
    pub compressed_pages: u64,
}

/// Creates [`GhosttyEmulator`]s with fixed [`Options`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GhosttyFactory {
    pub options: Options,
}

impl EmulatorFactory for GhosttyFactory {
    type Emulator = GhosttyEmulator;

    fn create(&mut self, size: GridSize) -> Result<GhosttyEmulator, Error> {
        GhosttyEmulator::new(size, &self.options)
    }
}

impl fmt::Debug for GhosttyEmulator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GhosttyEmulator")
            .field("size", &self.screen.size())
            .finish_non_exhaustive()
    }
}

/// Owns a terminal handle and frees it on drop.
struct TerminalHandle(NonNull<sys::GhosttyTerminalImpl>);

impl TerminalHandle {
    const fn as_ptr(&self) -> sys::GhosttyTerminal {
        self.0.as_ptr()
    }

    fn get_u16(&self, data: sys::GhosttyTerminalData) -> u16 {
        let mut value: u16 = 0;
        // SAFETY: the terminal is live; `data` documents a `uint16_t` output.
        check_contract("ghostty_terminal_get", unsafe {
            sys::ghostty_terminal_get(self.as_ptr(), data, (&raw mut value).cast())
        });
        value
    }

    fn get_bool(&self, data: sys::GhosttyTerminalData) -> bool {
        let mut value = false;
        // SAFETY: the terminal is live; `data` documents a `bool` output.
        check_contract("ghostty_terminal_get", unsafe {
            sys::ghostty_terminal_get(self.as_ptr(), data, (&raw mut value).cast())
        });
        value
    }

    fn get_usize(&self, data: sys::GhosttyTerminalData) -> usize {
        let mut value: usize = 0;
        // SAFETY: the terminal is live; `data` documents a `size_t` output,
        // which is `usize` on every target Rust supports.
        check_contract("ghostty_terminal_get", unsafe {
            sys::ghostty_terminal_get(self.as_ptr(), data, (&raw mut value).cast())
        });
        value
    }

    fn get_mode(&self, mode: sys::GhosttyMode) -> ModeState {
        let mut config = sys::GhosttyTerminalModeConfig { mode, value: false };
        // SAFETY: the terminal is live; `DATA_MODE` documents a
        // `GhosttyTerminalModeConfig` in/out value with `mode` initialised.
        check_contract("ghostty_terminal_get(MODE)", unsafe {
            sys::ghostty_terminal_get(
                self.as_ptr(),
                sys::GhosttyTerminalData::GHOSTTY_TERMINAL_DATA_MODE,
                (&raw mut config).cast(),
            )
        });
        config.value.into()
    }
}

impl Drop for TerminalHandle {
    fn drop(&mut self) {
        // SAFETY: this handle owns the terminal and frees it exactly once.
        unsafe { sys::ghostty_terminal_free(self.as_ptr()) };
    }
}

/// Where the callbacks put events until `feed` or `resize` drains them.
struct Sink {
    events: EmulatorEvents,
}

/// Owns the heap-allocated [`Sink`]. It is reached only through this raw
/// pointer (never through a `Box`), so the callbacks' accesses and ours never
/// alias a unique reference.
struct SinkBox(NonNull<Sink>);

impl SinkBox {
    fn new() -> Self {
        let sink = Box::new(Sink {
            events: EmulatorEvents::new(),
        });
        Self(NonNull::from(Box::leak(sink)))
    }

    const fn as_userdata(&self) -> *const c_void {
        self.0.as_ptr().cast_const().cast()
    }

    const fn get_mut(&mut self) -> &mut Sink {
        // SAFETY: the pointer came from a live Box; `&mut self` proves no
        // callback is running (they run only inside FFI calls made through
        // `&mut GhosttyEmulator`), so this is the only reference.
        unsafe { self.0.as_mut() }
    }
}

impl Drop for SinkBox {
    fn drop(&mut self) {
        // SAFETY: the pointer came from `Box::leak` in `new` and is reclaimed
        // exactly once; the terminal that could call back into it is already
        // freed (field order in `GhosttyEmulator`).
        drop(unsafe { Box::from_raw(self.0.as_ptr()) });
    }
}

/// A snapshot decoder; freed on drop, including on every error path.
struct Decoder(NonNull<sys::GhosttySnapshotDecoderImpl>);

impl Decoder {
    fn new(snapshot: &[u8]) -> Result<Self, Error> {
        const CALL: &str = "ghostty_snapshot_decoder_new_buf";
        let mut raw: sys::GhosttySnapshotDecoder = ptr::null_mut();
        // SAFETY: valid out-pointer; `snapshot` outlives the decoder because
        // the decoder never leaves `GhosttyEmulator::restore`.
        check(CALL, unsafe {
            sys::ghostty_snapshot_decoder_new_buf(
                crate::alloc::rust(),
                &raw mut raw,
                snapshot.as_ptr(),
                snapshot.len(),
            )
        })?;
        Ok(Self(adopt(raw, CALL)))
    }

    fn set(
        &self,
        call: &'static str,
        option: sys::GhosttySnapshotDecoderOption,
        value: *const c_void,
    ) -> Result<(), Error> {
        // SAFETY: the decoder is live and has not started decoding; every
        // caller passes a value of the type documented for `option`.
        check(call, unsafe {
            sys::ghostty_snapshot_decoder_set(self.0.as_ptr(), option, value)
        })
    }

    fn decode(self) -> Result<TerminalHandle, Error> {
        const CALL: &str = "ghostty_snapshot_decoder_decode";
        let mut raw: sys::GhosttyTerminal = ptr::null_mut();
        // SAFETY: the decoder is live and unused; valid out-pointer.
        check(CALL, unsafe {
            sys::ghostty_snapshot_decoder_decode(self.0.as_ptr(), &raw mut raw)
        })?;
        Ok(TerminalHandle(adopt(raw, CALL)))
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: this value owns the decoder and frees it exactly once.
        unsafe { sys::ghostty_snapshot_decoder_free(self.0.as_ptr()) };
    }
}

/// Collects query replies into the reused events buffer. Runs synchronously
/// inside `ghostty_terminal_vt_write` or `ghostty_terminal_resize`.
extern "C" fn on_write_pty(
    _terminal: sys::GhosttyTerminal,
    userdata: *mut c_void,
    data: *const u8,
    len: usize,
) {
    if len == 0 {
        return;
    }
    // SAFETY: see `sink_from`.
    let sink = unsafe { sink_from(userdata) };
    // SAFETY: libghostty-vt passes `len` readable bytes at `data`, valid for
    // the duration of the callback.
    let reply = unsafe { core::slice::from_raw_parts(data, len) };
    sink.events.push_reply(reply);
}

/// Records a bell. Runs synchronously inside `ghostty_terminal_vt_write`.
extern "C" fn on_bell(_terminal: sys::GhosttyTerminal, userdata: *mut c_void) {
    // SAFETY: see `sink_from`.
    let sink = unsafe { sink_from(userdata) };
    sink.events.ring_bell();
}

/// DA1: VT220 conformance with ANSI colour (`CSI ? 62 ; 22 c`), the same as
/// Ghostty's defaults. Clipboard support (52) is deliberately not claimed:
/// plx denies clipboard reads by default.
const DA1_CONFORMANCE_VT220: u16 = header_u16!(sys::GHOSTTY_DA_CONFORMANCE_VT220);
const DA1_FEATURE_ANSI_COLOR: u16 = header_u16!(sys::GHOSTTY_DA_FEATURE_ANSI_COLOR);
/// DA2: a VT220 (`CSI > 1 ; 0 ; 0 c`).
const DA2_DEVICE_TYPE_VT220: u16 = header_u16!(sys::GHOSTTY_DA_DEVICE_TYPE_VT220);

/// Answers device-attribute queries (DA1/DA2/DA3). Runs synchronously inside
/// `ghostty_terminal_vt_write`; the reply itself arrives via `on_write_pty`.
const extern "C" fn on_device_attributes(
    _terminal: sys::GhosttyTerminal,
    _userdata: *mut c_void,
    out: *mut sys::GhosttyDeviceAttributes,
) -> bool {
    let mut features = [0_u16; 64];
    features[0] = DA1_FEATURE_ANSI_COLOR;
    let attributes = sys::GhosttyDeviceAttributes {
        primary: sys::GhosttyDeviceAttributesPrimary {
            conformance_level: DA1_CONFORMANCE_VT220,
            features,
            num_features: 1,
        },
        secondary: sys::GhosttyDeviceAttributesSecondary {
            device_type: DA2_DEVICE_TYPE_VT220,
            firmware_version: 0,
            rom_cartridge: 0,
        },
        tertiary: sys::GhosttyDeviceAttributesTertiary { unit_id: 0 },
    };
    // SAFETY: libghostty-vt passes a valid, writable `GhosttyDeviceAttributes`.
    unsafe { out.write(attributes) };
    true
}

/// A program put something on the clipboard (OSC 52, or the iTerm2 and
/// kitty protocols, which libghostty normalizes): its text goes with the
/// feed's events, for the client's clipboard. Only the standard clipboard
/// and plain text are taken, up to [`MAX_CLIPBOARD_BYTES`]; a clear is a
/// write of nothing.
///
/// # Safety
///
/// libghostty-vt passes a live request, valid for the call, whose reply
/// function must be called during it.
unsafe extern "C" fn on_clipboard_write(
    _terminal: sys::GhosttyTerminal,
    userdata: *mut c_void,
    write: *const sys::GhosttyClipboardWrite,
) {
    use sys::GhosttyClipboardWriteResult as Outcome;
    // SAFETY: the caller's contract.
    let request = unsafe { &*write };
    // SAFETY: see `sink_from`.
    let sink = unsafe { sink_from(userdata) };
    let result =
        if request.location == sys::GhosttyClipboardLocation::GHOSTTY_CLIPBOARD_LOCATION_STANDARD {
            // SAFETY: `contents` holds `contents_len` borrowed representations.
            let contents = match request.contents_len {
                0 => &[][..],
                len => unsafe { core::slice::from_raw_parts(request.contents, len) },
            };
            let text = contents.iter().find(|content| {
                // SAFETY: the MIME string is borrowed for the call.
                unsafe { ghostty_bytes(content.mime) }.starts_with(b"text/plain")
            });
            match (request.contents_len, text) {
                (0, _) => {
                    sink.events.write_clipboard(b"");
                    Outcome::GHOSTTY_CLIPBOARD_WRITE_RESULT_SUCCESS
                }
                // SAFETY: the data string is borrowed for the call.
                (_, Some(text)) => match unsafe { ghostty_bytes(text.data) } {
                    data if data.len() > MAX_CLIPBOARD_BYTES => {
                        Outcome::GHOSTTY_CLIPBOARD_WRITE_RESULT_DENIED
                    }
                    data => {
                        sink.events.write_clipboard(data);
                        Outcome::GHOSTTY_CLIPBOARD_WRITE_RESULT_SUCCESS
                    }
                },
                (_, None) => Outcome::GHOSTTY_CLIPBOARD_WRITE_RESULT_UNSUPPORTED,
            }
        } else {
            // The primary selection and others: the client sets the clipboard.
            Outcome::GHOSTTY_CLIPBOARD_WRITE_RESULT_UNSUPPORTED
        };
    let reply = sys::GhosttyClipboardWriteReply {
        size: size_of::<sys::GhosttyClipboardWriteReply>(),
        result,
        remember: false,
    };
    if let Some(answer) = request.reply {
        // SAFETY: the request is live, and the reply borrowed for the call,
        // as the header requires.
        unsafe { answer(write, &raw const reply) };
    }
}

/// A program reported its progress (OSC 9;4): the report goes with the
/// feed's events, typed. A full reset (RIS) removes the report, and
/// libghostty says so through this callback too.
///
/// # Safety
///
/// libghostty-vt passes a live report, valid for the call.
unsafe extern "C" fn on_progress_report(
    _terminal: sys::GhosttyTerminal,
    userdata: *mut c_void,
    report: *const sys::GhosttyTerminalProgressReport,
) {
    use sys::GhosttyTerminalProgressState as State;
    // SAFETY: the caller's contract.
    let report = unsafe { &*report };
    contract!(
        report.size
            >= core::mem::offset_of!(sys::GhosttyTerminalProgressReport, progress)
                + size_of::<i8>(),
        "a progress report holds every field plx reads"
    );
    let percent = match u8::try_from(report.progress) {
        Ok(percent) => {
            Some(Percent::new(percent).or_contract("a progress report's percentage is at most 100"))
        }
        Err(_negative) => {
            contract!(
                report.progress == -1,
                "a progress report without a percentage says -1, not {}",
                report.progress
            );
            None
        }
    };
    let progress = match report.state {
        State::GHOSTTY_TERMINAL_PROGRESS_STATE_REMOVE => Progress::Removed,
        State::GHOSTTY_TERMINAL_PROGRESS_STATE_SET => Progress::Set(percent),
        State::GHOSTTY_TERMINAL_PROGRESS_STATE_ERROR => Progress::Error(percent),
        State::GHOSTTY_TERMINAL_PROGRESS_STATE_INDETERMINATE => Progress::Indeterminate,
        State::GHOSTTY_TERMINAL_PROGRESS_STATE_PAUSE => Progress::Paused(percent),
        other => breach!("the header lists every progress state, not {}", other.0),
    };
    // SAFETY: see `sink_from`.
    let sink = unsafe { sink_from(userdata) };
    sink.events.report_progress(progress);
}

/// The bytes of a borrowed libghostty string.
///
/// # Safety
///
/// `string` points to `len` bytes valid for the returned lifetime (or is
/// empty).
const unsafe fn ghostty_bytes<'a>(string: sys::GhosttyString) -> &'a [u8] {
    if string.len == 0 || string.ptr.is_null() {
        return &[];
    }
    // SAFETY: the caller's contract.
    unsafe { core::slice::from_raw_parts(string.ptr, string.len) }
}

/// # Safety
///
/// `userdata` must be the pointer installed by [`GhosttyEmulator::wire`],
/// and the caller must be a libghostty-vt callback. Callbacks run only inside
/// FFI calls made through `&mut GhosttyEmulator`, so no other reference to
/// the sink exists while the returned one is alive, and the sink outlives the
/// terminal.
unsafe fn sink_from<'a>(userdata: *mut c_void) -> &'a mut Sink {
    // SAFETY: guaranteed by the caller, per this function's contract.
    unsafe { &mut *userdata.cast::<Sink>() }
}

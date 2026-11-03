//! Selecting text with the mouse, through libghostty-vt's selection
//! gestures: a press starts (a double press selects a word, a triple one a
//! line), a drag extends, and the terminal holds the result as its
//! selection, which the render state reports per row and the formatter
//! turns into text.

use core::ptr::{self, NonNull};

use plx_emulator::{Autoscroll, CellPx, Past, Position, SelectAt, SelectGesture, Selection};
use plx_ghostty_sys as sys;

use crate::error::{Error, adopt, check, check_contract};

/// The cell size given to libghostty when the client reported none: it
/// maps positions through pixels, and cells come out exact whatever the
/// size.
const FALLBACK_CELL_PX: u32 = 10;

/// How soon a press must follow the last to count as a double or triple
/// press: the common desktop default.
const REPEAT_INTERVAL_NS: u64 = 500_000_000;

/// A pointer within this many pixels of the top or bottom edge
/// autoscrolls (libghostty's 1 px, and one more so a pointer in the pane is
/// never taken for one past it).
const AUTOSCROLL_EDGE_PX: f64 = 2.0;

/// A selection gesture and one reusable event per kind, owned by one
/// emulator.
pub(crate) struct Selector {
    gesture: GestureHandle,
    press: EventHandle,
    drag: EventHandle,
    release: EventHandle,
    tick: EventHandle,
    /// Where the last press was, to tell which way a drag goes.
    anchor: Position,
    /// Where the last drag was: autoscroll ticks extend the selection there.
    pointer: SelectAt,
}

/// The terminal a gesture applies to, as libghostty measures it.
#[derive(Clone, Copy)]
pub(crate) struct Surface {
    pub(crate) cols: u16,
    pub(crate) rows: u16,
    pub(crate) cell_px: Option<CellPx>,
}

impl Surface {
    /// The cell size libghostty maps positions through.
    fn cell(self) -> (u32, u32) {
        self.cell_px
            .map_or((FALLBACK_CELL_PX, FALLBACK_CELL_PX), |px| {
                (px.width.get(), px.height.get())
            })
    }

    /// `at`, clamped to a visible cell.
    fn clamp(self, at: SelectAt) -> SelectAt {
        SelectAt {
            cell: Position {
                x: at.cell.x.min(self.cols.saturating_sub(1)),
                y: at.cell.y.min(self.rows.saturating_sub(1)),
            },
            past: at.past,
        }
    }

    fn geometry(self) -> sys::GhosttySelectionGestureGeometry {
        let (cell_width, cell_height) = self.cell();
        sys::GhosttySelectionGestureGeometry {
            columns: u32::from(self.cols),
            cell_width,
            padding_left: 0,
            screen_height: u32::from(self.rows) * cell_height,
        }
    }
}

impl Selector {
    /// # Errors
    ///
    /// When libghostty-vt cannot create the gesture or its events.
    pub(crate) fn new() -> Result<Self, Error> {
        use sys::GhosttySelectionGestureEventType as Type;
        let nowhere = SelectAt {
            cell: Position { x: 0, y: 0 },
            past: Past::Nothing,
        };
        Ok(Self {
            gesture: GestureHandle::new()?,
            press: EventHandle::new(Type::GHOSTTY_SELECTION_GESTURE_EVENT_TYPE_PRESS)?,
            drag: EventHandle::new(Type::GHOSTTY_SELECTION_GESTURE_EVENT_TYPE_DRAG)?,
            release: EventHandle::new(Type::GHOSTTY_SELECTION_GESTURE_EVENT_TYPE_RELEASE)?,
            tick: EventHandle::new(Type::GHOSTTY_SELECTION_GESTURE_EVENT_TYPE_AUTOSCROLL_TICK)?,
            anchor: nowhere.cell,
            pointer: nowhere,
        })
    }

    /// Applies `gesture` with the pointer `at` on `surface`, at `time_ns`
    /// (for double and triple presses), and installs the selection it
    /// makes; a plain press clears it. Says whether the drag autoscrolls.
    pub(crate) fn gesture(
        &mut self,
        terminal: sys::GhosttyTerminal,
        gesture: SelectGesture,
        at: SelectAt,
        time_ns: u64,
        surface: Surface,
    ) -> Autoscroll {
        use sys::GhosttySelectionGestureEventOption as Opt;
        let at = surface.clamp(at);
        let grid_ref = grid_ref_at(terminal, at.cell);
        match gesture {
            SelectGesture::Press => self.anchor = at.cell,
            SelectGesture::Drag => self.pointer = at,
            SelectGesture::Release => {}
        }
        let position = self.position(gesture, at, surface);
        let geometry = surface.geometry();
        // Repeated presses within a cell of each other.
        let repeat_distance = f64::from(surface.cell().0);
        let repeat_interval = REPEAT_INTERVAL_NS;
        let event = match gesture {
            SelectGesture::Press => self.press.0.as_ptr(),
            SelectGesture::Drag => self.drag.0.as_ptr(),
            SelectGesture::Release => self.release.0.as_ptr(),
        };
        // SAFETY: the event is live and owned by `self`; each option's value
        // has the type its documentation names and lives across the calls
        // (the event copies what it keeps).
        unsafe {
            set(
                event,
                Opt::GHOSTTY_SELECTION_GESTURE_EVENT_OPT_REF,
                (&raw const grid_ref).cast(),
            );
            match gesture {
                SelectGesture::Press => {
                    set(
                        event,
                        Opt::GHOSTTY_SELECTION_GESTURE_EVENT_OPT_POSITION,
                        (&raw const position).cast(),
                    );
                    set(
                        event,
                        Opt::GHOSTTY_SELECTION_GESTURE_EVENT_OPT_TIME_NS,
                        (&raw const time_ns).cast(),
                    );
                    set(
                        event,
                        Opt::GHOSTTY_SELECTION_GESTURE_EVENT_OPT_REPEAT_INTERVAL_NS,
                        (&raw const repeat_interval).cast(),
                    );
                    set(
                        event,
                        Opt::GHOSTTY_SELECTION_GESTURE_EVENT_OPT_REPEAT_DISTANCE,
                        (&raw const repeat_distance).cast(),
                    );
                }
                SelectGesture::Drag => {
                    set(
                        event,
                        Opt::GHOSTTY_SELECTION_GESTURE_EVENT_OPT_POSITION,
                        (&raw const position).cast(),
                    );
                    set(
                        event,
                        Opt::GHOSTTY_SELECTION_GESTURE_EVENT_OPT_GEOMETRY,
                        (&raw const geometry).cast(),
                    );
                }
                SelectGesture::Release => {}
            }
        }
        let (result, selection) = self.apply(terminal, event);
        install(terminal, result, gesture, &selection);
        self.autoscroll(terminal)
    }

    /// One autoscroll tick on `surface`: libghostty scrolls the view a row
    /// towards the edge the last drag went past, and extends the selection
    /// to the row now under the pointer. Says whether it goes on.
    pub(crate) fn autoscroll_tick(
        &self,
        terminal: sys::GhosttyTerminal,
        surface: Surface,
    ) -> Autoscroll {
        use sys::GhosttySelectionGestureEventOption as Opt;
        // The terminal may have been resized since the drag.
        let at = surface.clamp(self.pointer);
        let viewport = sys::GhosttyPointCoordinate {
            x: at.cell.x,
            y: u32::from(at.cell.y),
        };
        let position = self.position(SelectGesture::Drag, at, surface);
        let geometry = surface.geometry();
        let event = self.tick.0.as_ptr();
        // SAFETY: as in `gesture`.
        unsafe {
            set(
                event,
                Opt::GHOSTTY_SELECTION_GESTURE_EVENT_OPT_VIEWPORT,
                (&raw const viewport).cast(),
            );
            set(
                event,
                Opt::GHOSTTY_SELECTION_GESTURE_EVENT_OPT_POSITION,
                (&raw const position).cast(),
            );
            set(
                event,
                Opt::GHOSTTY_SELECTION_GESTURE_EVENT_OPT_GEOMETRY,
                (&raw const geometry).cast(),
            );
        }
        let (result, selection) = self.apply(terminal, event);
        // A tick continues the drag: when it selects nothing (the drag is
        // over), the selection stays as it is.
        install(terminal, result, SelectGesture::Drag, &selection);
        self.autoscroll(terminal)
    }

    /// Applies `event` to the gesture: the result, and the selection it
    /// made when it made one.
    fn apply(
        &self,
        terminal: sys::GhosttyTerminal,
        event: sys::GhosttySelectionGestureEvent,
    ) -> (sys::GhosttyResult, sys::GhosttySelection) {
        let mut selection = empty_selection();
        // SAFETY: the gesture, terminal and event are live; `selection` is a
        // sized out-value.
        let result = unsafe {
            sys::ghostty_selection_gesture_event(
                self.gesture.0.as_ptr(),
                terminal,
                event,
                &raw mut selection,
            )
        };
        (result, selection)
    }

    /// Whether the gesture wants the view autoscrolled.
    fn autoscroll(&self, terminal: sys::GhosttyTerminal) -> Autoscroll {
        use sys::GhosttySelectionGestureAutoscroll as Wanted;
        let mut wanted = Wanted::GHOSTTY_SELECTION_GESTURE_AUTOSCROLL_NONE;
        // SAFETY: the gesture and terminal are live; the value has the type
        // the data field documents.
        check_contract("ghostty_selection_gesture_get(AUTOSCROLL)", unsafe {
            sys::ghostty_selection_gesture_get(
                self.gesture.0.as_ptr(),
                terminal,
                sys::GhosttySelectionGestureData::GHOSTTY_SELECTION_GESTURE_DATA_AUTOSCROLL,
                (&raw mut wanted).cast(),
            )
        });
        match wanted {
            Wanted::GHOSTTY_SELECTION_GESTURE_AUTOSCROLL_NONE => Autoscroll::Stopped,
            Wanted::GHOSTTY_SELECTION_GESTURE_AUTOSCROLL_UP
            | Wanted::GHOSTTY_SELECTION_GESTURE_AUTOSCROLL_DOWN => Autoscroll::Scrolling,
            other => plx_bug::breach!(
                "libghostty reported autoscroll {}, which it documents no meaning for",
                other.0,
            ),
        }
    }

    /// Where the pointer is, in the pixels libghostty takes. It includes the
    /// cell under a dragging pointer once the pointer is past the cell's
    /// middle, as pixel pointers move; in cells, the cell under the pointer
    /// is meant, so a drag reports the cell's far edge: the right one going
    /// forward from the press, the left one going back.
    ///
    /// Up and down, libghostty autoscrolls a drag within a pixel of the top
    /// edge or the bottom one: a dragging pointer past an edge is put on
    /// it, any other in its cell's middle, kept off the edges.
    fn position(
        &self,
        gesture: SelectGesture,
        at: SelectAt,
        surface: Surface,
    ) -> sys::GhosttySurfacePosition {
        let (width, height) = surface.cell();
        let forward = (at.cell.y, at.cell.x) >= (self.anchor.y, self.anchor.x);
        let x = match (gesture, forward) {
            (SelectGesture::Drag, true) => u32::from(at.cell.x) * width + width - 1,
            (SelectGesture::Drag, false) => u32::from(at.cell.x) * width,
            (SelectGesture::Press | SelectGesture::Release, _) => {
                u32::from(at.cell.x) * width + width / 2
            }
        };
        let screen_height = f64::from(surface.geometry().screen_height);
        let y = match (gesture, at.past) {
            (SelectGesture::Drag, Past::Top) => 0.0,
            (SelectGesture::Drag, Past::Bottom) => screen_height,
            (SelectGesture::Drag, Past::Nothing)
            | (SelectGesture::Press | SelectGesture::Release, _) => {
                f64::from(u32::from(at.cell.y) * height + height / 2)
                    .min(screen_height - AUTOSCROLL_EDGE_PX)
                    .max(AUTOSCROLL_EDGE_PX)
            }
        };
        sys::GhosttySurfacePosition { x: f64::from(x), y }
    }
}

/// The grid reference of the visible cell `at` of `terminal`.
fn grid_ref_at(terminal: sys::GhosttyTerminal, at: Position) -> sys::GhosttyGridRef {
    let mut grid_ref = sys::GhosttyGridRef {
        size: size_of::<sys::GhosttyGridRef>(),
        node: ptr::null_mut(),
        x: 0,
        y: 0,
    };
    let point = sys::GhosttyPoint {
        tag: sys::GhosttyPointTag::GHOSTTY_POINT_TAG_VIEWPORT,
        value: sys::GhosttyPointValue {
            coordinate: sys::GhosttyPointCoordinate {
                x: at.x,
                y: u32::from(at.y),
            },
        },
    };
    // SAFETY: the terminal is live; `coordinate` is the union field every
    // point tag reads; `grid_ref` is a sized out-value.
    check_contract("ghostty_terminal_grid_ref(VIEWPORT)", unsafe {
        sys::ghostty_terminal_grid_ref(terminal, point, &raw mut grid_ref)
    });
    grid_ref
}

/// Installs the selection a gesture made (`result`), or clears the old one
/// after a plain press.
fn install(
    terminal: sys::GhosttyTerminal,
    result: sys::GhosttyResult,
    gesture: SelectGesture,
    selection: &sys::GhosttySelection,
) {
    match (result, gesture) {
        (sys::GhosttyResult::GHOSTTY_SUCCESS, _) => {
            // SAFETY: the terminal is live; the selection's references
            // were just produced for its active screen, and the terminal
            // copies it at once.
            check_contract("ghostty_terminal_set(SELECTION)", unsafe {
                sys::ghostty_terminal_set(
                    terminal,
                    sys::GhosttyTerminalOption::GHOSTTY_TERMINAL_OPT_SELECTION,
                    core::ptr::from_ref(selection).cast(),
                )
            });
        }
        // A plain press selects nothing yet: the old selection goes.
        (sys::GhosttyResult::GHOSTTY_NO_VALUE, SelectGesture::Press) => clear(terminal),
        // A release never selects; a drag that changes nothing neither.
        (sys::GhosttyResult::GHOSTTY_NO_VALUE, SelectGesture::Drag | SelectGesture::Release) => {}
        (other, _) => check_contract("ghostty_selection_gesture_event", other),
    }
}

/// Clears `terminal`'s selection.
pub(crate) fn clear(terminal: sys::GhosttyTerminal) {
    // SAFETY: the terminal is live; a null value clears the selection.
    check_contract("ghostty_terminal_set(SELECTION, NULL)", unsafe {
        sys::ghostty_terminal_set(
            terminal,
            sys::GhosttyTerminalOption::GHOSTTY_TERMINAL_OPT_SELECTION,
            ptr::null(),
        )
    });
}

/// Appends `terminal`'s selected text to `out` (plain, soft wraps joined,
/// trailing blanks trimmed).
pub(crate) fn text(terminal: sys::GhosttyTerminal, out: &mut Vec<u8>) -> Selection {
    format(terminal, ptr::null(), out)
}

/// Appends all of `terminal`'s text, its history then its screen, to `out`
/// as [`text`] does.
pub(crate) fn all_text(terminal: sys::GhosttyTerminal, out: &mut Vec<u8>) {
    let mut all = empty_selection();
    // SAFETY: the terminal is live; `all` is a sized out-value.
    let result = unsafe { sys::ghostty_terminal_select_all(terminal, &raw mut all) };
    if result == sys::GhosttyResult::GHOSTTY_NO_VALUE {
        // Nothing selectable: no text at all.
        return;
    }
    check_contract("ghostty_terminal_select_all", result);
    let _text: Selection = format(terminal, &raw const all, out);
}

/// A selection out-value, before a call fills it in.
const fn empty_selection() -> sys::GhosttySelection {
    let nowhere = sys::GhosttyGridRef {
        size: size_of::<sys::GhosttyGridRef>(),
        node: ptr::null_mut(),
        x: 0,
        y: 0,
    };
    sys::GhosttySelection {
        size: size_of::<sys::GhosttySelection>(),
        start: nowhere,
        end: nowhere,
        rectangle: false,
    }
}

/// Appends the text of `selection` (null: the terminal's own) to `out`.
fn format(
    terminal: sys::GhosttyTerminal,
    selection: *const sys::GhosttySelection,
    out: &mut Vec<u8>,
) -> Selection {
    let options = sys::GhosttyTerminalSelectionFormatOptions {
        size: size_of::<sys::GhosttyTerminalSelectionFormatOptions>(),
        emit: sys::GhosttyFormatterFormat::GHOSTTY_FORMATTER_FORMAT_PLAIN,
        unwrap: true,
        trim: true,
        selection,
    };
    let mut bytes: *mut u8 = ptr::null_mut();
    let mut len = 0_usize;
    // SAFETY: the terminal is live; `selection` is null or was just
    // derived from its active screen; the out-pointers are valid; the
    // buffer is allocated with plx's allocator and freed with it below.
    let result = unsafe {
        sys::ghostty_terminal_selection_format_alloc(
            terminal,
            crate::alloc::rust(),
            options,
            &raw mut bytes,
            &raw mut len,
        )
    };
    if result == sys::GhosttyResult::GHOSTTY_NO_VALUE {
        return Selection::None;
    }
    check_contract("ghostty_terminal_selection_format_alloc", result);
    if !bytes.is_null() {
        // SAFETY: on success `bytes` holds `len` initialised bytes, owned
        // by us until freed.
        let formatted = unsafe { core::slice::from_raw_parts(bytes, len) };
        out.extend_from_slice(formatted);
        // SAFETY: allocated by the call above with this allocator and length.
        unsafe { sys::ghostty_free(crate::alloc::rust(), bytes, len) };
    }
    Selection::Some
}

/// Sets option `option` of `event` to `value`.
///
/// # Safety
///
/// `event` is live, and `value` points to the type `option` documents.
unsafe fn set(
    event: sys::GhosttySelectionGestureEvent,
    option: sys::GhosttySelectionGestureEventOption,
    value: *const core::ffi::c_void,
) {
    // SAFETY: the caller's contract.
    check_contract("ghostty_selection_gesture_event_set", unsafe {
        sys::ghostty_selection_gesture_event_set(event, option, value)
    });
}

/// An owned selection gesture.
struct GestureHandle(NonNull<sys::GhosttySelectionGestureImpl>);

impl GestureHandle {
    fn new() -> Result<Self, Error> {
        const CALL: &str = "ghostty_selection_gesture_new";
        let mut raw: sys::GhosttySelectionGesture = ptr::null_mut();
        // SAFETY: valid out-pointer and allocator.
        check(CALL, unsafe {
            sys::ghostty_selection_gesture_new(crate::alloc::rust(), &raw mut raw)
        })?;
        Ok(Self(adopt(raw, CALL)))
    }
}

impl Drop for GestureHandle {
    fn drop(&mut self) {
        // SAFETY: owned by this value and freed exactly once. The emulator
        // frees its terminal first (field order), so no terminal is passed:
        // its pages already released the gesture's tracked references.
        unsafe { sys::ghostty_selection_gesture_free(self.0.as_ptr(), ptr::null_mut()) };
    }
}

/// An owned selection gesture event of one fixed kind.
struct EventHandle(NonNull<sys::GhosttySelectionGestureEventImpl>);

impl EventHandle {
    fn new(kind: sys::GhosttySelectionGestureEventType) -> Result<Self, Error> {
        const CALL: &str = "ghostty_selection_gesture_event_new";
        let mut raw: sys::GhosttySelectionGestureEvent = ptr::null_mut();
        // SAFETY: valid out-pointer, allocator and event kind.
        check(CALL, unsafe {
            sys::ghostty_selection_gesture_event_new(crate::alloc::rust(), &raw mut raw, kind)
        })?;
        Ok(Self(adopt(raw, CALL)))
    }
}

impl Drop for EventHandle {
    fn drop(&mut self) {
        // SAFETY: owned by this value and freed exactly once.
        unsafe { sys::ghostty_selection_gesture_event_free(self.0.as_ptr()) };
    }
}

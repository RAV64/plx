//! What must hold, checked from outside: a client's terminal shows its
//! mirror, carries nothing but the compositor's own sequences, and at
//! quiescence every mirror equals the daemon's real screens.
//!
//! Failures are values, not panics, so a property test can shrink the
//! schedule that caused them.

use plx_client_core::mirror::{Mirror, MirrorPane};
use plx_core::geometry::Axis;
use plx_core::session::Session;
use plx_core::size::pane_area;
use plx_emulator::{
    CellText, CursorShape, CursorVisibility, Emulator, GlyphKind, GridSize, Position, Style, Width,
};
use plx_ghostty::{GhosttyEmulator, GhosttyFactory};

/// A check that did not hold, with what was seen.
pub type Failure = String;

/// A cell as it looks; a blank is the same whether erased or a space.
/// Clusters are borrowed from whichever tables hold them.
#[derive(Debug, PartialEq, Eq)]
struct Look<'a> {
    text: Text<'a>,
    width: Width,
    style: Style,
}

#[derive(Debug, PartialEq, Eq)]
enum Text<'a> {
    Blank,
    Char(char),
    Cluster(&'a [char]),
}

const BLANK: Look<'static> = Look {
    text: Text::Blank,
    width: Width::Narrow,
    style: Style::DEFAULT,
};

const fn text_of(text: CellText<'_>) -> Text<'_> {
    match text {
        CellText::Empty | CellText::Char(' ') => Text::Blank,
        CellText::Char(c) => Text::Char(c),
        CellText::Cluster(cluster) => Text::Cluster(cluster),
    }
}

/// The style a separator cell at (`x`, `y`) must have: the focus colour
/// where it borders the focused pane `focus`.
fn border_style(axis: Axis, x: u16, y: u16, focus: Option<plx_core::geometry::Rect>) -> Style {
    let borders = focus.is_some_and(|f| {
        let (right, bottom) = (f.x() + f.cols(), f.y() + f.rows());
        match axis {
            Axis::Horizontal => (x == right || x + 1 == f.x()) && f.y() <= y && y < bottom,
            Axis::Vertical => (y == bottom || y + 1 == f.y()) && f.x() <= x && x < right,
        }
    });
    if borders {
        plx_client_core::compose::BORDER_FOCUSED
    } else {
        plx_client_core::compose::BORDER
    }
}

/// Where separator lines meet, joins them, judging by the cells drawn: a
/// `│` connects up and down to any line, left and right only to a `─`; a
/// `─` the other way round.
fn join_lines(screen: &mut [Vec<Option<Look<'_>>>]) {
    let line_at = |screen: &[Vec<Option<Look<'_>>>], x: Option<usize>, y: Option<usize>| {
        let look = screen.get(y?)?.get(x?)?.as_ref()?;
        if let Text::Char(c @ ('│' | '─')) = look.text {
            Some(c)
        } else {
            None
        }
    };
    let mut joined = Vec::new();
    for (y, row) in screen.iter().enumerate() {
        for x in 0..row.len() {
            let Some(line) = line_at(screen, Some(x), Some(y)) else {
                continue;
            };
            let near = |dx: isize, dy: isize| {
                line_at(screen, x.checked_add_signed(dx), y.checked_add_signed(dy))
            };
            let column = line == '│';
            let across = if column { '─' } else { '│' };
            let any = |c: Option<char>| c.is_some();
            let crossing = |c: Option<char>| c == Some(across);
            let (up, down, left, right) = if column {
                (
                    any(near(0, -1)),
                    any(near(0, 1)),
                    crossing(near(-1, 0)),
                    crossing(near(1, 0)),
                )
            } else {
                (
                    crossing(near(0, -1)),
                    crossing(near(0, 1)),
                    any(near(-1, 0)),
                    any(near(1, 0)),
                )
            };
            let glyph = match (up, down, left, right) {
                (true, true, true, true) => '┼',
                (true, true, false, true) => '├',
                (true, true, true, false) => '┤',
                (false, true, true, true) => '┬',
                (true, false, true, true) => '┴',
                (false, true, false, true) => '┌',
                (false, true, true, false) => '┐',
                (true, false, false, true) => '└',
                (true, false, true, false) => '┘',
                _ => line,
            };
            joined.push((x, y, glyph));
        }
    }
    for (x, y, glyph) in joined {
        if let Some(Some(look)) = screen.get_mut(y).and_then(|row| row.get_mut(x)) {
            look.text = Text::Char(glyph);
        }
    }
}

/// What the terminal must show for `mirror`: `None` where the check does
/// not say (the status line, checked by its text).
fn expected<'m>(
    mirror: &'m Mirror,
    terminal: GridSize,
) -> Result<Vec<Vec<Option<Look<'m>>>>, Failure> {
    let header = mirror.header().ok_or("a drawn mirror has a frame")?;
    let cols = terminal.cols().get().min(header.size.cols().get());
    let rows = terminal.rows().get().min(header.size.rows().get());
    let mut screen: Vec<Vec<Option<Look<'m>>>> = (0..terminal.rows().get())
        .map(|_| (0..terminal.cols().get()).map(|_| Some(BLANK)).collect())
        .collect();
    let mut put = |x: u16, y: u16, look: Option<Look<'m>>| {
        if let Some(cell) = screen
            .get_mut(usize::from(y))
            .and_then(|row| row.get_mut(usize::from(x)))
        {
            *cell = look;
        }
    };
    let status = pane_area(header.size).rows();
    if status < header.size.rows().get() && status < rows {
        for x in 0..cols {
            put(x, status, None);
        }
    }
    let focus = mirror.pane_or_none(header.focus).map(MirrorPane::rect);
    for separator in mirror.separators() {
        let rect = separator.rect;
        let glyph = match separator.axis {
            Axis::Horizontal => '│',
            Axis::Vertical => '─',
        };
        for y in rect.y()..(rect.y() + rect.rows()).min(rows) {
            for x in rect.x()..(rect.x() + rect.cols()).min(cols) {
                put(
                    x,
                    y,
                    Some(Look {
                        text: Text::Char(glyph),
                        width: Width::Narrow,
                        style: border_style(separator.axis, x, y, focus),
                    }),
                );
            }
        }
    }
    join_lines(&mut screen);
    let mut put = |x: u16, y: u16, look: Option<Look<'m>>| {
        if let Some(cell) = screen
            .get_mut(usize::from(y))
            .and_then(|row| row.get_mut(usize::from(x)))
        {
            *cell = look;
        }
    };
    for (_, pane) in mirror.panes() {
        let rect = pane.rect();
        let right = (rect.x() + rect.cols()).min(cols);
        for y in 0..rect.rows() {
            let row = rect.y() + y;
            if row >= rows {
                break;
            }
            let cells = pane.row_or_none(y).ok_or("a pane's rows fill its rect")?;
            for (x, cell) in (rect.x()..right).zip(cells) {
                let style = pane.style(cell.style);
                let text = match cell.glyph.kind() {
                    GlyphKind::Empty | GlyphKind::Char(' ') => Text::Blank,
                    GlyphKind::Char(c) => Text::Char(c),
                    GlyphKind::Cluster(id) => Text::Cluster(pane.grapheme(id)),
                };
                let look = match cell.width {
                    // Cut by the clip, or a wrap placeholder: a blank.
                    Width::Wide if x + 1 >= right => Look {
                        text: Text::Blank,
                        width: Width::Narrow,
                        style,
                    },
                    Width::SpacerHead => Look {
                        text: Text::Blank,
                        width: Width::Narrow,
                        style,
                    },
                    Width::Narrow | Width::Wide | Width::SpacerTail => Look {
                        text,
                        width: cell.width,
                        style,
                    },
                };
                put(x, row, Some(look));
            }
        }
    }
    Ok(screen)
}

/// The client's terminal shows its mirror: every cell drawn (text, width,
/// style), blanks elsewhere, the mode at the start of the status line, and
/// the focused pane's cursor.
///
/// # Errors
///
/// The first difference.
pub fn shows_mirror(
    mirror: &Mirror,
    terminal: GridSize,
    outer: &GhosttyEmulator,
) -> Result<(), Failure> {
    let Some(header) = mirror.header() else {
        return Ok(());
    };
    let screen = outer.screen();
    for (y, (wanted, row)) in expected(mirror, terminal)?
        .iter()
        .zip(screen.grid().rows())
        .enumerate()
    {
        for (x, (wanted, &cell)) in wanted.iter().zip(row).enumerate() {
            let shown = screen.resolve(cell);
            let look = Look {
                text: text_of(shown.text),
                width: shown.width,
                style: *shown.style,
            };
            if let Some(wanted) = wanted
                && look != *wanted
            {
                return Err(format!(
                    "at column {x}, row {y}: shows {look:?}, wanted {wanted:?}"
                ));
            }
        }
    }
    let status = pane_area(header.size).rows();
    if status < header.size.rows().get() && status < terminal.rows().get() {
        let line: String = screen
            .grid()
            .row_or_none(status)
            .ok_or("the status row is on screen")?
            .iter()
            .map(|&cell| match screen.resolve(cell).text {
                CellText::Char(c) => c,
                CellText::Empty | CellText::Cluster(_) => ' ',
            })
            .collect();
        let mode = format!(" {:?} ", header.mode).to_uppercase();
        let shown = mode.chars().count().min(usize::from(
            terminal.cols().get().min(header.size.cols().get()),
        ));
        if !line.chars().take(shown).eq(mode.chars().take(shown)) {
            return Err(format!(
                "the status line {line:?} does not start with {mode:?}"
            ));
        }
    }
    let cursor = screen.grid().cursor();
    let placed = mirror.pane_or_none(header.focus).and_then(|pane| {
        let (rect, wanted) = (pane.rect(), pane.cursor());
        let position = wanted.position?;
        let x = rect.x() + position.x;
        let y = rect.y() + position.y;
        let inside = position.x < rect.cols()
            && position.y < rect.rows()
            && x < terminal.cols().get().min(header.size.cols().get())
            && y < terminal.rows().get().min(header.size.rows().get());
        (inside && wanted.visibility == CursorVisibility::Visible)
            .then_some((Position { x, y }, wanted))
    });
    match placed {
        Some((position, wanted)) => {
            // The terminal's own cursor blinks as the terminal says.
            let blink_differs =
                wanted.shape != CursorShape::Default && cursor.blink != wanted.blink;
            if cursor.visibility != CursorVisibility::Visible
                || cursor.position != Some(position)
                || cursor.shape != wanted.shape
                || blink_differs
            {
                return Err(format!(
                    "the cursor is {cursor:?}, wanted {wanted:?} at {position:?}"
                ));
            }
        }
        None if cursor.visibility != CursorVisibility::Hidden => {
            return Err(format!(
                "the cursor shows ({cursor:?}) with nothing to show"
            ));
        }
        None => {}
    }
    Ok(())
}

/// The rest of an OSC after `ESC ]` at byte `at`, which must be one the
/// client encodes itself, ended by `ESC \`: a clipboard write (`52;c;`,
/// base64) or a progress report (`9;4;` and a state, with a percentage up
/// to 100 for the states that have one).
fn client_osc(chars: &mut core::str::CharIndices<'_>, at: usize) -> Result<(), Failure> {
    let not_ours = || format!("an OSC at byte {at} that is not the client's own");
    let mut body = String::new();
    loop {
        match chars.next().map(|(_, c)| c) {
            Some('\u{1b}') if chars.next().map(|(_, c)| c) == Some('\\') => break,
            Some(c) if !c.is_control() => body.push(c),
            _ => return Err(not_ours()),
        }
    }
    if is_clipboard_write(&body) || is_progress_report(&body) {
        Ok(())
    } else {
        Err(not_ours())
    }
}

/// `52;c;` and base64.
fn is_clipboard_write(body: &str) -> bool {
    body.strip_prefix("52;c;").is_some_and(|base64| {
        base64
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '='))
    })
}

/// `9;4;` and a state, with a percentage up to 100 for the states that
/// have one.
fn is_progress_report(body: &str) -> bool {
    let percent = |p: &str| {
        p.len() <= 3
            && p.bytes().all(|b| b.is_ascii_digit())
            && p.parse::<u8>().is_ok_and(|p| p <= 100)
    };
    body.strip_prefix("9;4;")
        .is_some_and(|report| match report.split_once(';') {
            None => matches!(report, "0" | "1" | "2" | "3" | "4"),
            Some((state, p)) => matches!(state, "1" | "2" | "4") && percent(p),
        })
}

/// Everything a client writes to its terminal is text or one of the
/// compositor's own sequences.
///
/// Those are synchronized output, cursor visibility, autowrap, erase
/// display, cursor position, SGR and cursor style, and the client's own
/// OSCs, ended by ST: clipboard writes (OSC 52 to the clipboard, base64;
/// the client encodes them from the text a selection or a program copied)
/// and progress reports (OSC 9;4, rebuilt from the typed report). A pane's
/// canaries (RIS, scroll regions, the alternate screen, OSC titles, its raw
/// clipboard writes and progress reports, bells) can only reach the
/// terminal by failing this.
///
/// # Errors
///
/// The first byte or sequence outside the vocabulary.
pub fn in_vocabulary(bytes: &[u8]) -> Result<(), Failure> {
    let text = core::str::from_utf8(bytes)
        .map_err(|error| format!("terminal output is not UTF-8: {error}"))?;
    let mut chars = text.char_indices();
    while let Some((at, c)) = chars.next() {
        match c {
            '\u{1b}' => {
                match chars.next().map(|(_, c)| c) {
                    Some('[') => {}
                    Some(']') => {
                        client_osc(&mut chars, at)?;
                        continue;
                    }
                    _ => return Err(format!("a non-CSI escape at byte {at}")),
                }
                let mut params = String::new();
                let mut intermediates = String::new();
                let mut last = None;
                for (_, c) in chars.by_ref() {
                    match u32::from(c) {
                        0x30..=0x3F if intermediates.is_empty() => params.push(c),
                        0x20..=0x2F => intermediates.push(c),
                        0x40..=0x7E => {
                            last = Some(c);
                            break;
                        }
                        _ => return Err(format!("a malformed CSI at byte {at}")),
                    }
                }
                let last = last.ok_or_else(|| format!("an unterminated CSI at byte {at}"))?;
                let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
                let allowed = match (last, intermediates.as_str()) {
                    ('h' | 'l', "") => matches!(params.as_str(), "?2026" | "?25" | "?7"),
                    ('J', "") => params == "2",
                    ('H', "") => params.split(';').count() == 2 && params.split(';').all(digits),
                    ('m', "") => params
                        .bytes()
                        .all(|b| b.is_ascii_digit() || b == b';' || b == b':'),
                    ('q', " ") => {
                        matches!(params.as_str(), "0" | "1" | "2" | "3" | "4" | "5" | "6")
                    }
                    _ => false,
                };
                if !allowed {
                    return Err(format!(
                        "CSI {params}{intermediates}{last} at byte {at} is not the compositor's"
                    ));
                }
            }
            c if c.is_control() => {
                return Err(format!(
                    "control character {:#x} at byte {at}",
                    u32::from(c)
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

/// At quiescence a client's mirror is the daemon's truth: the session's
/// size, its active tab's arrangement and focus, and every shown pane's
/// cells (compared by look: text, width, style) and cursor.
///
/// # Errors
///
/// The first difference.
pub fn matches_session(mirror: &Mirror, session: &Session<GhosttyFactory>) -> Result<(), Failure> {
    let header = mirror
        .header()
        .ok_or("an attached client has drawn a frame")?;
    if header.size != session.size() {
        return Err(format!(
            "mirror size {:?}, session {:?}",
            header.size,
            session.size()
        ));
    }
    let tab = session.structure().active();
    if header.focus != tab.focus() {
        return Err(format!(
            "mirror focus {:?}, session {:?}",
            header.focus,
            tab.focus()
        ));
    }
    let arrangement = tab.arrange(pane_area(session.size()));
    if mirror.placements() != arrangement.panes.as_slice() {
        return Err(format!(
            "placements {:?}, session {:?}",
            mirror.placements(),
            arrangement.panes
        ));
    }
    if mirror.separators() != arrangement.separators.as_slice() {
        return Err(format!(
            "separators {:?}, session {:?}",
            mirror.separators(),
            arrangement.separators
        ));
    }
    let shown = arrangement
        .panes
        .iter()
        .filter(|placement| placement.rect.is_some())
        .count();
    if mirror.panes().count() != shown {
        return Err(format!(
            "the mirror holds {} panes, {shown} are shown",
            mirror.panes().count()
        ));
    }
    for (id, pane) in mirror.panes() {
        let screen = session.pane_screen_or_none(id).ok_or_else(|| {
            format!("the mirror shows pane {id:?}, which the session does not have")
        })?;
        if screen.grid().cursor() != pane.cursor() {
            return Err(format!(
                "pane {id:?}: cursor {:?}, session {:?}",
                pane.cursor(),
                screen.grid().cursor()
            ));
        }
        for (y, truth) in (0_u16..).zip(screen.grid().rows()) {
            let row = pane
                .row_or_none(y)
                .ok_or_else(|| format!("pane {id:?}: the mirror lacks row {y}"))?;
            for (x, (&mine, &theirs)) in row.iter().zip(truth).enumerate() {
                let theirs = screen.resolve(theirs);
                let mine_text = match mine.glyph.kind() {
                    GlyphKind::Empty => CellText::Empty,
                    GlyphKind::Char(c) => CellText::Char(c),
                    GlyphKind::Cluster(cluster) => CellText::Cluster(pane.grapheme(cluster)),
                };
                if text_of(mine_text) != text_of(theirs.text)
                    || mine.width != theirs.width
                    || pane.style(mine.style) != *theirs.style
                {
                    return Err(format!(
                        "pane {id:?} cell ({x}, {y}): {mine:?} vs {theirs:?}"
                    ));
                }
            }
        }
    }
    Ok(())
}

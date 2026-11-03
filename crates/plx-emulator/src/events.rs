//! What a program asked for while its output was fed: a typed summary,
//! reused between feeds (cleared, never reallocated in steady state).

use alloc::vec::Vec;

use crate::progress::Progress;

/// Whether the program rang the bell. How often does not matter: a client
/// shows one bell per frame at most.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Bell {
    #[default]
    Quiet,
    Rang,
}

/// The most a program may put on the clipboard at once: more is refused
/// (the program is told so where its protocol answers).
pub const MAX_CLIPBOARD_BYTES: usize = 8 << 20;

/// Whether the program put text on the clipboard.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Clipboard {
    #[default]
    Untouched,
    Written,
}

/// The side effects of one `feed` (or `resize`).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct EmulatorEvents {
    replies: Vec<u8>,
    bell: Bell,
    /// The text the program last put on the clipboard (OSC 52), if it did.
    clipboard: Vec<u8>,
    copied: Clipboard,
    /// The program's last progress report (OSC 9;4), if it made one.
    progress: Option<Progress>,
}

impl EmulatorEvents {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            replies: Vec::new(),
            bell: Bell::Quiet,
            clipboard: Vec::new(),
            copied: Clipboard::Untouched,
            progress: None,
        }
    }

    /// Forgets the previous feed's events, keeping the buffer's capacity.
    pub fn clear(&mut self) {
        self.replies.clear();
        self.bell = Bell::Quiet;
        self.clipboard.clear();
        self.copied = Clipboard::Untouched;
        self.progress = None;
    }

    /// Records text the program put on the clipboard; a later write in the
    /// same feed replaces it.
    pub fn write_clipboard(&mut self, text: &[u8]) {
        self.clipboard.clear();
        self.clipboard.extend_from_slice(text);
        self.copied = Clipboard::Written;
    }

    /// The text the program put on the clipboard during this feed, if any.
    #[must_use]
    pub fn clipboard_or_none(&self) -> Option<&[u8]> {
        match self.copied {
            Clipboard::Written => Some(&self.clipboard),
            Clipboard::Untouched => None,
        }
    }

    /// Records a progress report; a later one in the same feed replaces it.
    pub const fn report_progress(&mut self, progress: Progress) {
        self.progress = Some(progress);
    }

    /// The program's last progress report during this feed, if it made one.
    /// A report that repeats the last one still counts: a terminal drops a
    /// progress bar that is not renewed.
    #[must_use]
    pub const fn progress_or_none(&self) -> Option<Progress> {
        self.progress
    }

    /// Appends bytes the terminal answered with.
    pub fn push_reply(&mut self, bytes: &[u8]) {
        self.replies.extend_from_slice(bytes);
    }

    /// Records a bell.
    pub const fn ring_bell(&mut self) {
        self.bell = Bell::Rang;
    }

    /// Every reply, concatenated in stream order: write them to the pane's
    /// PTY as one byte string.
    #[must_use]
    pub fn replies(&self) -> &[u8] {
        &self.replies
    }

    #[must_use]
    pub const fn bell(&self) -> Bell {
        self.bell
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.replies.is_empty()
            && self.bell == Bell::Quiet
            && self.copied == Clipboard::Untouched
            && self.progress.is_none()
    }
}

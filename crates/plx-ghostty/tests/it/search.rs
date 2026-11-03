//! Searching the history: what matches, how it is drawn, and the view
//! following the selected match.

use plx_emulator::{
    Case, Color, Count, Emulator, Rgb, SearchDirection, SearchOptions, SearchStatus, Words,
};
use plx_ghostty::GhosttyEmulator;

const MATCH_BG: Color = Color::Rgb(Rgb {
    r: 0xa0,
    g: 0x93,
    b: 0x84,
});
const SELECTED_BG: Color = Color::Rgb(Rgb {
    r: 0xea,
    g: 0x98,
    b: 0x75,
});

/// Each row as drawn: `m` a match's cell, `s` the selected match's, `.`
/// any other; trailing dots dropped.
fn marks(emulator: &mut GhosttyEmulator) -> Vec<String> {
    crate::view(emulator)
        .rows
        .iter()
        .map(|row| {
            let marks: String = row
                .iter()
                .map(|(_, _, style)| {
                    if style.bg == MATCH_BG {
                        'm'
                    } else if style.bg == SELECTED_BG {
                        's'
                    } else {
                        '.'
                    }
                })
                .collect();
            String::from(marks.trim_end_matches('.'))
        })
        .collect()
}

fn status(total: u32, selected: Option<u32>, options: SearchOptions) -> SearchStatus {
    SearchStatus {
        options,
        total,
        selected: selected.map(|n| n.try_into().unwrap()),
        count: Count::Current,
    }
}

fn options(case: Case, words: Words) -> SearchOptions {
    SearchOptions { case, words }
}

#[test]
fn matches_are_marked_and_selected_newest_first() {
    let mut emulator = crate::emulator(crate::size(20, 4));
    emulator.feed(b"foo Foo food\r\nbar foo");
    let any = SearchOptions::default();
    emulator.search("foo", any);
    assert_eq!(emulator.search_status(), status(4, None, any));
    assert_eq!(marks(&mut emulator), ["mmm.mmm.mmm", "....mmm", "", ""]);
    // From the live screen up: the newest match first.
    emulator.search_select(SearchDirection::Older);
    assert_eq!(emulator.search_status(), status(4, Some(1), any));
    assert_eq!(marks(&mut emulator), ["mmm.mmm.mmm", "....sss", "", ""]);
    emulator.search_select(SearchDirection::Older);
    assert_eq!(emulator.search_status(), status(4, Some(2), any));
    // And back down, past the newest round to the oldest.
    emulator.search_select(SearchDirection::Newer);
    emulator.search_select(SearchDirection::Newer);
    assert_eq!(emulator.search_status(), status(4, Some(4), any));
    assert_eq!(marks(&mut emulator), ["sss.mmm.mmm", "....mmm", "", ""]);
    // An empty needle stops searching.
    emulator.search("", any);
    assert_eq!(emulator.search_status(), status(0, None, any));
    assert_eq!(marks(&mut emulator), ["", "", "", ""]);
}

#[test]
fn letter_case_and_whole_words_filter_the_matches() {
    let mut emulator = crate::emulator(crate::size(20, 4));
    emulator.feed(b"foo Foo food\r\nbar foo");
    let case = options(Case::Matched, Words::Within);
    emulator.search("foo", case);
    assert_eq!(emulator.search_status(), status(3, None, case));
    assert_eq!(marks(&mut emulator), ["mmm.....mmm", "....mmm", "", ""]);
    let words = options(Case::Ignored, Words::Whole);
    emulator.search("foo", words);
    assert_eq!(emulator.search_status(), status(3, None, words));
    assert_eq!(marks(&mut emulator), ["mmm.mmm", "....mmm", "", ""]);
    let both = options(Case::Matched, Words::Whole);
    emulator.search("foo", both);
    assert_eq!(emulator.search_status(), status(2, None, both));
    // Selecting skips what the options do not accept: the oldest match
    // the options take, then round to the newest.
    emulator.search_select(SearchDirection::Older);
    emulator.search_select(SearchDirection::Older);
    assert_eq!(emulator.search_status(), status(2, Some(2), both));
    assert_eq!(marks(&mut emulator), ["sss", "....mmm", "", ""]);
    emulator.search_select(SearchDirection::Older);
    assert_eq!(emulator.search_status(), status(2, Some(1), both));
    // Nothing accepted: nothing selected.
    emulator.search("FOO", case);
    emulator.search_select(SearchDirection::Older);
    assert_eq!(emulator.search_status(), status(0, None, case));
}

#[test]
fn a_match_wrapped_onto_two_rows_is_marked_on_both() {
    let mut emulator = crate::emulator(crate::size(10, 4));
    emulator.feed(b"01234567needle");
    let case = options(Case::Matched, Words::Within);
    emulator.search("needle", case);
    assert_eq!(emulator.search_status(), status(1, None, case));
    assert_eq!(marks(&mut emulator), ["........mm", "mmmm", "", ""]);
}

#[test]
fn selecting_a_match_in_the_history_scrolls_to_it() {
    let mut emulator = crate::emulator(crate::size(20, 4));
    emulator.feed(b"a needle here\r\n");
    for n in 0..40 {
        emulator.feed(format!("line {n}\r\n").as_bytes());
    }
    let any = SearchOptions::default();
    emulator.search("needle", any);
    assert_eq!(emulator.search_status(), status(1, None, any));
    assert_eq!(emulator.scroll_position().back, 0);
    emulator.search_select(SearchDirection::Older);
    assert_eq!(emulator.search_status(), status(1, Some(1), any));
    assert!(emulator.scroll_position().back > 0);
    let shown = crate::view(&mut emulator).text();
    let row = shown
        .iter()
        .position(|text| text == "a needle here")
        .unwrap_or_else(|| panic!("the match is in view: {shown:?}"));
    assert_eq!(marks(&mut emulator)[row], "..ssssss");
}

/// Editing the scrollback starts from all of it: the history, then the
/// screen, soft wraps joined, trailing blanks trimmed.
#[test]
fn the_history_text_holds_history_and_screen() {
    let mut emulator = crate::emulator(crate::size(10, 4));
    emulator.feed(b"first line\r\n0123456789abc\r\n");
    for n in 0..6 {
        emulator.feed(format!("line {n}   \r\n").as_bytes());
    }
    emulator.feed(b"$ ");
    let mut out = Vec::new();
    emulator.history_text(&mut out);
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.starts_with("first line\n0123456789abc\nline 0\nline 1\n"),
        "{text:?}"
    );
    assert!(text.trim_end().ends_with("line 5\n$"), "{text:?}");
}

/// Whole words as `grep -w`: no letter, digit or `_` right before or after;
/// punctuation and blanks are boundaries, a wide character next to it is a
/// letter like any, and across a soft wrap the line goes on.
#[test]
fn whole_words_are_bounded_by_non_word_characters() {
    let words = options(Case::Ignored, Words::Whole);
    let mut emulator = crate::emulator(crate::size(20, 4));
    emulator.feed("foo-bar _foo foo_ (foo)\r\n漢foo foo漢 2foo".as_bytes());
    emulator.search("foo", words);
    // "foo-bar", "(foo)": yes; "_foo", "foo_", "漢foo", "foo漢", "2foo": no.
    assert_eq!(emulator.search_status(), status(2, None, words));
    // A word cut by the row's end goes on in the next row (10 columns).
    let mut emulator = crate::emulator(crate::size(10, 4));
    emulator.feed(b"012345 foo\r\n012345 foobar");
    emulator.search("foo", words);
    // The first ends its line; the second goes on as "foobar".
    assert_eq!(emulator.search_status(), status(1, None, words));
    assert_eq!(marks(&mut emulator), [".......mmm", "", "", ""]);
}

/// A filtered count with many matches is not taken again for each piece of
/// output: it is marked stale until the next search key.
#[test]
fn a_large_filtered_count_waits_for_the_next_search_key() {
    let case = options(Case::Matched, Words::Within);
    let mut emulator = crate::emulator(crate::size(40, 10));
    for _ in 0..1100 {
        emulator.feed(b"xyz xyz xyz xyz\r\n");
    }
    emulator.search("xyz", case);
    assert_eq!(emulator.search_status(), status(4400, None, case));
    emulator.feed(b"xyz\r\n");
    crate::view(&mut emulator);
    let stale = emulator.search_status();
    assert_eq!((stale.total, stale.count), (4400, Count::Stale));
    emulator.search_select(SearchDirection::Older);
    assert_eq!(emulator.search_status(), status(4401, Some(1), case));
    // Few matches: counted again as output comes.
    let mut emulator = crate::emulator(crate::size(40, 10));
    emulator.feed(b"xyz\r\n");
    emulator.search("xyz", case);
    emulator.feed(b"xyz\r\n");
    crate::view(&mut emulator);
    assert_eq!(emulator.search_status(), status(2, None, case));
}

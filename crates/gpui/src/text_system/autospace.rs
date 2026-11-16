use crate::{FontRun, Pixels, px};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use unicode_script::{Script, UnicodeScript};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthChar;

/// Contextual spacing between directly adjoining typographic character units.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum TextAutospace {
    /// Preserve the native advances without inter-script spacing.
    #[default]
    None,
    /// Add one eighth of a CJK advance between Han and narrow Latin/digit units.
    IdeographAlphaNumeric,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Ideograph,
    AlphaNumeric,
    Other,
}

fn class(grapheme: &str) -> Class {
    let Some(base) = grapheme.chars().next() else {
        return Class::Other;
    };
    if base.script() == Script::Han && base.is_alphabetic() {
        Class::Ideograph
    } else if base.width() == Some(1)
        && (base.is_ascii_digit() || (base.script() == Script::Latin && base.is_alphabetic()))
    {
        Class::AlphaNumeric
    } else {
        Class::Other
    }
}

/// The original byte boundary and the adjacent Han unit used to measure CJK advance.
pub(super) fn boundaries(text: &str) -> impl Iterator<Item = (usize, usize)> + '_ {
    let mut previous = (0, Class::Other);
    text.grapheme_indices(true)
        .filter_map(move |(byte, grapheme)| {
            let current = class(grapheme);
            let boundary = match (previous.1, current) {
                (Class::Ideograph, Class::AlphaNumeric) => Some((byte, previous.0)),
                (Class::AlphaNumeric, Class::Ideograph) => Some((byte, byte)),
                _ => None,
            };
            previous = (byte, current);
            boundary
        })
}

pub(super) fn enabled_boundaries<'a>(
    text: &'a str,
    runs: &'a [FontRun],
) -> impl Iterator<Item = (usize, usize)> + 'a {
    let mut run_index = 0;
    let mut run_start = 0;
    boundaries(text).filter(move |&(byte, _)| {
        while runs
            .get(run_index)
            .is_some_and(|run| run_start + run.len < byte)
        {
            run_start += runs[run_index].len;
            run_index += 1;
        }
        let Some(left) = runs.get(run_index) else {
            return false;
        };
        let right = if run_start + left.len == byte {
            runs.get(run_index + 1)
        } else {
            Some(left)
        };
        left.text_autospace == TextAutospace::IdeographAlphaNumeric
            && right.is_some_and(|run| run.text_autospace == TextAutospace::IdeographAlphaNumeric)
    })
}

#[derive(Clone, Copy)]
pub(super) struct Spacing {
    pub byte: usize,
    pub advance: Pixels,
}

pub(super) fn advance_at(
    spacing: &mut std::iter::Peekable<impl Iterator<Item = Spacing>>,
    byte: usize,
) -> Pixels {
    while spacing.peek().is_some_and(|space| space.byte < byte) {
        spacing.next();
    }
    if spacing.peek().is_some_and(|space| space.byte == byte) {
        spacing.next().map_or(px(0.), |space| space.advance)
    } else {
        px(0.)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autospace_boundaries_preserve_original_graphemes_and_bytes() {
        let text = "甲Rust2026乙";
        assert_eq!(
            boundaries(text).map(|entry| entry.0).collect::<Vec<_>>(),
            ["甲".len(), "甲Rust2026".len()]
        );
        for text in ["甲e\u{301}乙", "𠀀\u{e0100}Rust乙"] {
            let positions: Vec<_> = text.grapheme_indices(true).map(|entry| entry.0).collect();
            let boundaries: Vec<_> = boundaries(text).collect();
            assert_eq!(boundaries.len(), 2);
            assert!(
                boundaries
                    .iter()
                    .all(|&(byte, _)| positions.contains(&byte))
            );
        }
    }

    #[test]
    fn autospace_does_not_expand_existing_spaces_punctuation_or_emoji() {
        for text in [
            "中文",
            "Rust2026",
            "甲 Rust 乙",
            "甲\u{a0}A",
            "甲，A",
            "甲（A）乙",
            "甲👩‍👩‍👧‍👦乙",
            "甲Ａ１２乙",
            "甲\nA",
            "甲\u{fffc}乙",
        ] {
            assert_eq!(boundaries(text).count(), 0, "{text}");
        }
    }

    #[test]
    fn autospace_requires_both_sides_to_opt_in_across_runs() {
        let text = "甲Rust乙";
        let mut runs = [
            FontRun {
                len: 3,
                font_id: crate::FontId(0),
                text_autospace: TextAutospace::IdeographAlphaNumeric,
            },
            FontRun {
                len: 4,
                font_id: crate::FontId(1),
                text_autospace: TextAutospace::None,
            },
            FontRun {
                len: 3,
                font_id: crate::FontId(0),
                text_autospace: TextAutospace::IdeographAlphaNumeric,
            },
        ];
        assert_eq!(enabled_boundaries(text, &runs).count(), 0);
        runs[1].text_autospace = TextAutospace::IdeographAlphaNumeric;
        assert_eq!(
            enabled_boundaries(text, &runs)
                .map(|entry| entry.0)
                .collect::<Vec<_>>(),
            [3, 7]
        );
    }
}

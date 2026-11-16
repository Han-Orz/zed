use crate::{Pixels, px};
use unicode_linebreak::{BreakClass, break_property, linebreaks};
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum BreakKind {
    Normal,
    Emergency,
    Prohibited,
}

/// UAX #14 decides linguistic opportunities; graphemes constrain every split.
/// Emergency breaks only divide oversized words or runs of ordinary spaces,
/// never punctuation, nonbreaking glue or a joined emoji.
pub(super) fn opportunities(text: &str) -> impl Iterator<Item = (usize, BreakKind)> + '_ {
    let mut normal = linebreaks(text).peekable();
    let mut previous_word = false;
    let mut previous_space = false;
    text.grapheme_indices(true).map(move |(byte, grapheme)| {
        while normal.peek().is_some_and(|&(index, _)| index < byte) {
            normal.next();
        }
        let word = grapheme.chars().next().is_some_and(|character| {
            matches!(
                break_property(character as u32),
                BreakClass::Alphabetic
                    | BreakClass::HebrewLetter
                    | BreakClass::Numeric
                    | BreakClass::ComplexContext
            )
        });
        let space = grapheme == " ";
        let kind = if byte == 0 {
            BreakKind::Prohibited
        } else if normal.peek().is_some_and(|&(index, _)| index == byte) {
            BreakKind::Normal
        } else if (previous_word && word) || (previous_space && space) {
            BreakKind::Emergency
        } else {
            BreakKind::Prohibited
        };
        previous_word = word;
        previous_space = space;
        (byte, kind)
    })
}

pub(super) struct BreakPoint<T> {
    pub byte: usize,
    pub x: Pixels,
    pub boundary: Option<T>,
    pub kind: BreakKind,
}

/// Both measured characters and shaped glyph clusters use this greedy fit.
/// With no fitting opportunity, an indivisible group overflows as one group;
/// breaking a closing mark or a grapheme is not a narrow-width escape hatch.
pub(super) fn fit<T: Copy>(
    points: &[BreakPoint<T>],
    width: Pixels,
    indentation: Option<(usize, Pixels, Pixels, u32)>,
    max_breaks: usize,
) -> Vec<(T, u32)> {
    let mut result = Vec::new();
    let mut start_byte = 0;
    let mut start_x = px(0.);
    let mut indent = None;
    let mut normal = None;
    let mut emergency = None;
    let mut scan = 0;
    while scan < points.len() && result.len() < max_breaks {
        let point = &points[scan];
        if point.byte <= start_byte {
            scan += 1;
            continue;
        }
        let indent_width = indentation.map_or(px(0.), |(_, _, space_width, _)| {
            space_width * indent.unwrap_or(0) as f32
        });
        if point.x - start_x + indent_width <= width {
            if point.boundary.is_none() {
                break;
            }
            match point.kind {
                BreakKind::Normal
                    if indentation.is_none_or(|(first, _, _, _)| point.byte > first) =>
                {
                    normal = Some(scan)
                }
                BreakKind::Normal => emergency = Some(scan),
                BreakKind::Emergency => emergency = Some(scan),
                BreakKind::Prohibited => {}
            }
            scan += 1;
            continue;
        }
        let chosen = normal.or(emergency).unwrap_or(scan);
        let chosen_point = &points[chosen];
        let Some(boundary) = chosen_point.boundary else {
            break;
        };
        if indent.is_none()
            && let Some((first_byte, first_x, _, maximum)) = indentation
            && first_x <= point.x
        {
            indent = Some(first_byte.saturating_sub(start_byte).min(maximum as usize) as u32);
        }
        result.push((boundary, indent.unwrap_or(0)));
        start_byte = chosen_point.byte;
        start_x = chosen_point.x;
        normal = None;
        emergency = None;
        scan = chosen + 1;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wrap(text: &str, columns: f32) -> Vec<&str> {
        let points: Vec<_> = opportunities(text)
            .enumerate()
            .filter(|(_, (_, kind))| *kind != BreakKind::Prohibited)
            .map(|(column, (byte, kind))| BreakPoint {
                byte,
                x: px(column as f32),
                boundary: Some(byte),
                kind,
            })
            .chain(std::iter::once(BreakPoint {
                byte: text.len(),
                x: px(text.graphemes(true).count() as f32),
                boundary: None,
                kind: BreakKind::Normal,
            }))
            .collect();
        let mut start = 0;
        let mut lines = Vec::new();
        for (end, _) in fit(&points, px(columns), None, usize::MAX) {
            lines.push(&text[start..end]);
            start = end;
        }
        lines.push(&text[start..]);
        lines
    }

    #[test]
    fn unicode_wrap_keeps_marks_and_graphemes_together() {
        assert_eq!(wrap("你好，世界。", 2.), ["你", "好，", "世", "界。"]);
        assert_eq!(wrap("甲（乙丙）丁", 3.), ["甲（乙", "丙）丁"]);
        for text in ["甲……乙", "甲——乙", "甲？！乙", "甲e\u{301}乙", "甲👩‍👩‍👧‍👦乙"]
        {
            let lines = wrap(text, 1.);
            assert_eq!(lines.concat(), text);
            assert!(lines.iter().all(|line| !line.is_empty()));
            assert!(
                lines
                    .iter()
                    .all(|line| !line.starts_with(['…', '？', '！', '\u{301}', '\u{200d}']))
            );
            assert!(
                lines
                    .iter()
                    .all(|line| !line.starts_with('—') || line.starts_with("——"))
            );
        }
    }

    #[test]
    fn unicode_wrap_uses_emergency_only_for_overlong_words() {
        assert_eq!(wrap("aa bbbb c", 5.), ["aa ", "bbbb ", "c"]);
        assert_eq!(wrap("abcdefgh", 3.), ["abc", "def", "gh"]);
        assert_eq!(wrap("甲（乙）丙", 0.), ["甲", "（乙）", "丙"]);
        assert_eq!(wrap("a\u{a0}b c", 1.), ["a\u{a0}b ", "c"]);
    }
}

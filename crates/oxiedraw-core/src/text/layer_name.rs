//! Layer name derived from a text layer's text: one line, at most 20 visible
//! characters, invisible formatting and stacked diacritics (zalgo) removed.

const MAX_NAME_CHARS: usize = 20;
const MAX_STACKED_MARKS: usize = 2;
const ZERO_WIDTH_JOINER: char = '\u{200D}';
const BLACK_FLAG: char = '\u{1F3F4}';

#[must_use]
pub fn layer_name_from_text(text: &str) -> Option<String> {
    let mut clusters: Vec<String> = Vec::new();
    let mut pending_space = false;
    let mut stacked_marks = 0;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_whitespace() {
            pending_space = !clusters.is_empty();
            continue;
        }
        if c.is_control() || is_hidden(c) {
            continue;
        }
        let attaches = !pending_space && !clusters.is_empty();
        if c == ZERO_WIDTH_JOINER {
            if attaches
                && let Some(&next) = chars.peek()
                && !next.is_whitespace()
                && !next.is_control()
                && !is_hidden(next)
            {
                let last = clusters.last_mut().expect("attaches checks non-empty");
                last.push(c);
                last.push(next);
                chars.next();
            }
            continue;
        }
        if is_extender(c) || is_tag(c) || is_stacking_mark(c) {
            let Some(last) = clusters.last_mut().filter(|_| attaches) else {
                continue;
            };
            if is_tag(c) && !last.starts_with(BLACK_FLAG) {
                continue;
            }
            if is_stacking_mark(c) {
                if stacked_marks >= MAX_STACKED_MARKS {
                    continue;
                }
                stacked_marks += 1;
            }
            last.push(c);
            continue;
        }
        if is_regional_indicator(c)
            && attaches
            && let Some(last) = clusters.last_mut()
            && last.chars().count() == 1
            && last.chars().all(is_regional_indicator)
        {
            last.push(c);
            continue;
        }
        let needed = usize::from(pending_space) + 1;
        if clusters.len() + needed > MAX_NAME_CHARS {
            break;
        }
        if pending_space {
            clusters.push(" ".to_string());
            pending_space = false;
        }
        clusters.push(c.to_string());
        stacked_marks = 0;
    }
    let name: String = clusters.concat();
    (!name.is_empty()).then_some(name)
}

// General-purpose diacritic blocks that zalgo text piles up; script-specific
// marks (Thai, Devanagari, ...) are left alone.
const fn is_stacking_mark(c: char) -> bool {
    matches!(c,
        '\u{0300}'..='\u{034E}'
        | '\u{0350}'..='\u{036F}'
        | '\u{1AB0}'..='\u{1AFF}'
        | '\u{1DC0}'..='\u{1DFF}'
        | '\u{20D0}'..='\u{20FF}'
        | '\u{FE20}'..='\u{FE2F}')
}

const fn is_extender(c: char) -> bool {
    matches!(c,
        '\u{200C}'
        | '\u{FE00}'..='\u{FE0F}'
        | '\u{E0100}'..='\u{E01EF}'
        | '\u{1F3FB}'..='\u{1F3FF}')
}

const fn is_tag(c: char) -> bool {
    matches!(c, '\u{E0020}'..='\u{E007F}')
}

const fn is_regional_indicator(c: char) -> bool {
    matches!(c, '\u{1F1E6}'..='\u{1F1FF}')
}

const fn is_hidden(c: char) -> bool {
    matches!(c,
        '\u{00AD}'
        | '\u{034F}'
        | '\u{061C}'
        | '\u{115F}'..='\u{1160}'
        | '\u{17B4}'..='\u{17B5}'
        | '\u{180E}'
        | '\u{200B}'
        | '\u{200E}'..='\u{200F}'
        | '\u{202A}'..='\u{202E}'
        | '\u{2060}'..='\u{206F}'
        | '\u{3164}'
        | '\u{E000}'..='\u{F8FF}'
        | '\u{FDD0}'..='\u{FDEF}'
        | '\u{FEFF}'
        | '\u{FFA0}'
        | '\u{FFF0}'..='\u{FFFF}'
        | '\u{1D173}'..='\u{1D17A}'
        | '\u{E0001}'
        | '\u{F0000}'..='\u{10FFFF}')
}


#[cfg(test)]
mod tests {
    use super::*;

    fn name(text: &str) -> Option<String> {
        layer_name_from_text(text)
    }

    #[test]
    fn plain_text_is_kept() {
        assert_eq!(name("Hello"), Some("Hello".into()));
    }

    #[test]
    fn long_text_is_cut_at_twenty_without_ellipsis() {
        assert_eq!(name("abcdefghijklmnopqrstuvwxyz"), Some("abcdefghijklmnopqrst".into()));
    }

    #[test]
    fn whitespace_collapses_to_single_spaces_on_one_line() {
        assert_eq!(name("  one \n\n two\tthree  "), Some("one two three".into()));
    }

    #[test]
    fn a_cut_never_ends_on_a_space() {
        assert_eq!(name("abcdefghijklmnopqrs tuv"), Some("abcdefghijklmnopqrs".into()));
    }

    #[test]
    fn zalgo_marks_are_capped() {
        let zalgo = "Z\u{0351}\u{036B}\u{0343}\u{036A}\u{0302}\u{036B}a\u{0310}\u{0352}";
        assert_eq!(name(zalgo), Some("Z\u{0351}\u{036B}a\u{0310}\u{0352}".into()));
    }

    #[test]
    fn accents_survive() {
        assert_eq!(name("Cafe\u{0301}"), Some("Cafe\u{0301}".into()));
        let vietnamese = "Ti\u{1EBF}ng Vi\u{1EC7}t";
        assert_eq!(name(vietnamese).as_deref(), Some(vietnamese));
    }

    #[test]
    fn other_scripts_survive() {
        let samples = [
            "\u{3053}\u{3093}\u{306B}\u{3061}\u{306F}\u{4E16}\u{754C}",
            "\u{41F}\u{440}\u{438}\u{432}\u{435}\u{442}",
            "\u{645}\u{631}\u{62D}\u{628}\u{627}",
            "\u{928}\u{92E}\u{938}\u{94D}\u{924}\u{947}",
            "\u{E2A}\u{E27}\u{E31}\u{E2A}\u{E14}\u{E35}",
            "\u{C548}\u{B155}\u{D558}\u{C138}\u{C694}",
            "\u{2813}\u{2811}\u{2807}\u{2807}\u{2815}",
        ];
        for text in samples {
            assert_eq!(name(text).as_deref(), Some(text));
        }
    }

    #[test]
    fn emoji_sequences_stay_whole_and_count_once() {
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
        let flag = "\u{1F1FA}\u{1F1E6}";
        let thumbs = "\u{1F44D}\u{1F3FD}";
        let text = format!("{family}{flag}{thumbs}");
        assert_eq!(name(&text), Some(text.clone()));
        let long = format!("{}{family}", "a".repeat(19));
        assert_eq!(name(&long), Some(format!("{}{family}", "a".repeat(19))));
    }

    #[test]
    fn invisible_formatting_is_dropped() {
        let text = "\u{202E}abc\u{200B}d\u{FEFF}e\u{E000}";
        assert_eq!(name(text), Some("abcde".into()));
    }

    #[test]
    fn marks_without_a_base_are_dropped() {
        assert_eq!(name("\u{0301}\u{0301} x \u{0301}"), Some("x".into()));
    }

    #[test]
    fn nothing_visible_gives_none() {
        assert_eq!(name(""), None);
        assert_eq!(name(" \n\t\u{200B}\u{0300}"), None);
    }
}

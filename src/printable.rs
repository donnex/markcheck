//! Strips the characters a terminal would act on rather than show, for text
//! from a checklist, a file name or git that markcheck writes to the
//! terminal (or into a commit message) outside ratatui, which drops them by
//! itself.

/// Control characters (C0, DEL and C1) and the bidi controls that reorder
/// the text around them.
pub fn is_hidden(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
        )
}

/// `text` without hidden characters, keeping its line breaks and tabs, for
/// multi-line output such as the git-sync exit report or a commit body.
pub fn printable(text: &str) -> String {
    text.chars()
        .filter(|&c| c == '\n' || c == '\t' || !is_hidden(c))
        .collect()
}

/// `text` on one line: hidden characters dropped, line breaks and tabs turned
/// into spaces, for the terminal title, the exit line and a commit subject.
pub fn printable_line(text: &str) -> String {
    text.chars()
        .filter_map(|c| match c {
            '\n' | '\t' => Some(' '),
            c if is_hidden(c) => None,
            c => Some(c),
        })
        .collect()
}

#[cfg(test)]
#[path = "printable_tests.rs"]
mod tests;

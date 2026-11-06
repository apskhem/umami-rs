//! The JavaScript string semantics the ported libraries depend on.

/// `str.substring(0, max)`: truncates to `max` UTF-16 code units. A surrogate pair
/// split by the cut is dropped entirely, since Rust strings can't hold half of one.
pub fn truncate(s: &str, max: usize) -> &str {
    let mut units = 0;

    for (index, c) in s.char_indices() {
        units += c.len_utf16();
        if units > max {
            return &s[..index];
        }
    }

    s
}

/// JS `\s` / `String.prototype.trim` whitespace.
pub fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{9}'..='\u{d}'
            | ' '
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_by_utf16_units() {
        assert_eq!(truncate("abc", 2), "ab");
        assert_eq!(truncate("a😀b", 2), "a");
        assert_eq!(truncate("a😀b", 3), "a😀");
        assert_eq!(truncate("abc", 10), "abc");
    }
}

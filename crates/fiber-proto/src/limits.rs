//! Size bounds that both ends of the agent protocol have to agree on.
//!
//! A cap that only one side applies is not a cap: the agent trims a line the server would
//! have stored whole, or the server stores a line the agent believed it had trimmed. Both
//! read the constant from here, and both call [`truncate_log_line`], so "64 KiB" means the
//! same number of bytes in the same place on either side of the socket.

/// The most bytes one log line may carry on the wire and into `log_lines.data`.
///
/// A step that prints a 100 MB "line" (a `tar` to stdout, a minified bundle, a binary) used
/// to become a WebSocket frame past tungstenite's 64 MiB default — which closes the socket,
/// taking every other step on that agent with it — and a 100 MB row nobody can read. The
/// output past the cap is lost on purpose: what is kept is enough to see what the step was
/// doing, and the marker says the rest was there.
pub const MAX_LOG_LINE_BYTES: usize = 64 * 1024;

/// Appended to a line that was cut, so the gap is visible rather than silent.
pub const TRUNCATION_MARKER: &str = "…[truncated]";

/// `s` cut to [`MAX_LOG_LINE_BYTES`] including the marker, or `s` unchanged.
///
/// Borrowing on the common path matters: this runs on every line of every step on both
/// sides, and the overwhelming majority are short.
pub fn truncate_log_line(s: &str) -> std::borrow::Cow<'_, str> {
    if s.len() <= MAX_LOG_LINE_BYTES {
        return std::borrow::Cow::Borrowed(s);
    }
    let keep = floor_char_boundary(s, MAX_LOG_LINE_BYTES - TRUNCATION_MARKER.len());
    let mut out = String::with_capacity(keep + TRUNCATION_MARKER.len());
    out.push_str(&s[..keep]);
    out.push_str(TRUNCATION_MARKER);
    std::borrow::Cow::Owned(out)
}

/// The largest index `<= at` that splits `s` between characters.
///
/// `str::floor_char_boundary` is still unstable. Slicing a `String` mid-character panics,
/// and a log line is the one place where arbitrary bytes arrive by design.
pub fn floor_char_boundary(s: &str, at: usize) -> usize {
    if at >= s.len() {
        return s.len();
    }
    let mut i = at;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Bytes of one raw line to keep before decoding, leaving room for the marker.
///
/// The agent cuts the *bytes* it read from the pipe, before `from_utf8_lossy` turns each
/// invalid byte into a three-byte replacement character and pushes the result past the cap
/// again. Decoding first and truncating after would work too, but it means allocating the
/// whole 100 MB line first, which is the thing the cap exists to avoid.
pub const MAX_RAW_LINE_BYTES: usize = MAX_LOG_LINE_BYTES - TRUNCATION_MARKER.len();

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_line_is_borrowed_unchanged() {
        let s = "hello";
        assert!(matches!(
            truncate_log_line(s),
            std::borrow::Cow::Borrowed("hello")
        ));
    }

    #[test]
    fn a_line_at_the_cap_is_left_alone() {
        let s = "x".repeat(MAX_LOG_LINE_BYTES);
        assert!(matches!(
            truncate_log_line(&s),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    #[test]
    fn a_line_past_the_cap_is_cut_to_the_cap_including_the_marker() {
        let s = "x".repeat(MAX_LOG_LINE_BYTES * 3);
        let out = truncate_log_line(&s);
        assert!(out.ends_with(TRUNCATION_MARKER));
        assert_eq!(
            out.len(),
            MAX_LOG_LINE_BYTES,
            "the result must fit the cap, marker included"
        );
    }

    #[test]
    fn cutting_never_splits_a_character() {
        // A multi-byte character straddling the cut: slicing at the raw byte index would
        // panic, so the boundary has to move back.
        let mut s = "é".repeat(MAX_LOG_LINE_BYTES); // 2 bytes each
        s.push_str("tail");
        let out = truncate_log_line(&s);
        assert!(out.ends_with(TRUNCATION_MARKER));
        assert!(out.len() <= MAX_LOG_LINE_BYTES);
        assert!(
            out.strip_suffix(TRUNCATION_MARKER)
                .is_some_and(|k| k.chars().all(|c| c == 'é'))
        );
    }

    #[test]
    fn floor_char_boundary_matches_the_std_semantics() {
        assert_eq!(floor_char_boundary("héllo", 2), 1); // inside the é
        assert_eq!(floor_char_boundary("héllo", 3), 3);
        assert_eq!(floor_char_boundary("abc", 99), 3);
        assert_eq!(floor_char_boundary("", 5), 0);
    }
}

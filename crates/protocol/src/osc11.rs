//! OSC 11 terminal-background probe handling (spec 0073).
//!
//! Child sessions ask "what is the terminal background color?" with
//! `OSC 11 ; ? (BEL|ST)`. When a connected client paints its own frame
//! background (dark/light UI themes), the daemon — the single authority in
//! front of every child PTY — answers with that painted color and strips the
//! query from the downstream byte stream so no attached terminal emulator
//! (e.g. xterm.js in the web client) answers a second time.
//!
//! The scanner is a pure chunk-fed state machine: a query can be split
//! across PTY read chunks, so an ambiguous trailing prefix of a query is
//! carried in `tail` and withheld from passthrough until the next chunk
//! resolves it one way or the other.

/// BEL-terminated background query.
pub const QUERY_BEL: &[u8] = b"\x1b]11;?\x07";
/// ST-terminated background query.
pub const QUERY_ST: &[u8] = b"\x1b]11;?\x1b\\";

/// Feed one PTY output chunk. Returns the passthrough bytes (the chunk with
/// every complete query removed and any ambiguous trailing query-prefix
/// withheld into `tail`) and the number of complete queries found.
///
/// `tail` must be reused across consecutive chunks of the same stream; it
/// only ever holds a proper prefix of a query (at most 7 bytes).
pub fn scan_and_strip_queries(tail: &mut Vec<u8>, chunk: &[u8]) -> (Vec<u8>, usize) {
    let mut combined = std::mem::take(tail);
    combined.extend_from_slice(chunk);

    let mut passthrough = Vec::with_capacity(combined.len());
    let mut count = 0usize;
    let mut i = 0usize;
    while i < combined.len() {
        let rest = &combined[i..];
        if rest.starts_with(QUERY_BEL) {
            count += 1;
            i += QUERY_BEL.len();
            continue;
        }
        if rest.starts_with(QUERY_ST) {
            count += 1;
            i += QUERY_ST.len();
            continue;
        }
        // A trailing proper prefix of either query form is ambiguous until
        // the next chunk arrives — withhold it. Anything else passes through.
        if is_query_prefix(rest) {
            *tail = rest.to_vec();
            return (passthrough, count);
        }
        passthrough.push(combined[i]);
        i += 1;
    }
    (passthrough, count)
}

/// True when `bytes` is a proper prefix of one of the query forms (i.e. it
/// could still become a query once more bytes arrive).
fn is_query_prefix(bytes: &[u8]) -> bool {
    !bytes.is_empty()
        && ((bytes.len() < QUERY_BEL.len() && QUERY_BEL.starts_with(bytes))
            || (bytes.len() < QUERY_ST.len() && QUERY_ST.starts_with(bytes)))
}

/// Build the OSC 11 reply for an 8-bit-per-channel background color, using
/// the 16-bit-per-channel `rgb:RRRR/GGGG/BBBB` form terminals answer with.
pub fn response_bytes((r, g, b): (u8, u8, u8)) -> Vec<u8> {
    format!(
        "\x1b]11;rgb:{:04x}/{:04x}/{:04x}\x07",
        r as u16 * 257,
        g as u16 * 257,
        b as u16 * 257
    )
    .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_plain_output_through_untouched() {
        let mut tail = Vec::new();
        let (out, n) = scan_and_strip_queries(&mut tail, b"hello world\x1b[31mred\x1b[0m");
        assert_eq!(out, b"hello world\x1b[31mred\x1b[0m");
        assert_eq!(n, 0);
        assert!(tail.is_empty());
    }

    #[test]
    fn strips_bel_and_st_queries_and_counts_them() {
        let mut tail = Vec::new();
        let (out, n) = scan_and_strip_queries(&mut tail, b"a\x1b]11;?\x07b\x1b]11;?\x1b\\c");
        assert_eq!(out, b"abc");
        assert_eq!(n, 2);
        assert!(tail.is_empty());
    }

    #[test]
    fn withholds_split_query_until_resolved() {
        let mut tail = Vec::new();
        let (out, n) = scan_and_strip_queries(&mut tail, b"x\x1b]11;?");
        assert_eq!(out, b"x");
        assert_eq!(n, 0);
        assert_eq!(tail, b"\x1b]11;?");

        let (out, n) = scan_and_strip_queries(&mut tail, b"\x07y");
        assert_eq!(out, b"y");
        assert_eq!(n, 1);
        assert!(tail.is_empty());
    }

    #[test]
    fn split_st_terminator_across_three_chunks() {
        let mut tail = Vec::new();
        let (out, n) = scan_and_strip_queries(&mut tail, b"\x1b]11");
        assert!(out.is_empty());
        assert_eq!(n, 0);
        let (out, n) = scan_and_strip_queries(&mut tail, b";?\x1b");
        assert!(out.is_empty());
        assert_eq!(n, 0);
        let (out, n) = scan_and_strip_queries(&mut tail, b"\\done");
        assert_eq!(out, b"done");
        assert_eq!(n, 1);
        assert!(tail.is_empty());
    }

    #[test]
    fn flushes_false_prefix_instead_of_eating_it() {
        let mut tail = Vec::new();
        let (out, n) = scan_and_strip_queries(&mut tail, b"\x1b]11;");
        assert!(out.is_empty());
        assert_eq!(tail, b"\x1b]11;");
        // Next byte disproves the query — everything must reappear.
        let (out, n2) = scan_and_strip_queries(&mut tail, b"xrest");
        assert_eq!(n + n2, 0);
        assert_eq!(out, b"\x1b]11;xrest");
        assert!(tail.is_empty());
    }

    #[test]
    fn lone_esc_at_chunk_end_is_withheld_then_flushed() {
        let mut tail = Vec::new();
        let (out, _) = scan_and_strip_queries(&mut tail, b"line\x1b");
        assert_eq!(out, b"line");
        assert_eq!(tail, b"\x1b");
        let (out, _) = scan_and_strip_queries(&mut tail, b"[2Jmore");
        assert_eq!(out, b"\x1b[2Jmore");
        assert!(tail.is_empty());
    }

    #[test]
    fn response_uses_16bit_channels() {
        assert_eq!(
            response_bytes((0x0c, 0x12, 0x1b)),
            b"\x1b]11;rgb:0c0c/1212/1b1b\x07".to_vec()
        );
        assert_eq!(
            response_bytes((0xff, 0x00, 0x80)),
            b"\x1b]11;rgb:ffff/0000/8080\x07".to_vec()
        );
    }
}

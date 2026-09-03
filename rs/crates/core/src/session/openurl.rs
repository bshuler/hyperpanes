//! The pane-side half of URL interception (T10): a pure, state-free scanner for the
//! OSC a CLI tool's "open this link" is turned into.
//!
//! A tool that wants to show the user a page runs whatever `$BROWSER` names. Every
//! pane's pty exports `BROWSER` pointing at the shim [`crate::open::ensure_browser_shim`]
//! writes, and the shim's whole job is to write
//!
//! ```text
//! ESC ] 1337 ; HyperpanesOpenURL = <url> BEL
//! ```
//!
//! to its own `/dev/tty` — so the URL arrives on the same byte stream the pane is
//! already reading, needing no socket, no port and no shared token, and arriving
//! pre-attributed to the pane that asked for it. `1337` is iTerm2's extensible
//! namespace, so it is the key name that makes the sequence ours; another terminal
//! reading this stream ignores an unknown `1337` key, and no other tool can collide
//! with `HyperpanesOpenURL`.
//!
//! **This scanner is the trust boundary.** Any process holding the pane's tty can emit
//! this sequence — a printed log line is enough — so what comes out of here is
//! untrusted input that happens to be well-framed, not a request from the shim.
//! Everything that is not a plain `http`/`https`/`mailto` URL, is over-long, or carries
//! a control character is dropped right here rather than downstream, because this is
//! the only place that knows the bytes came off a pty.
//!
//! Modelled on [`crate::session::cwd::parse_osc_cwd`]: same bounded carry, same
//! BEL-or-ST terminators, same fast reject. It differs in one way that matters — it
//! returns *every* URL in the window rather than the last, because two tools opening a
//! link during one read must both be routed, and a cwd is a state (last wins) where an
//! open is an event (none may be dropped).

/// Bound on a carried, still-incomplete OSC sequence (matches `cwd::OSC_MAX`).
const OSC_MAX: usize = 8192;
const OSC_PREFIX: &str = "\u{1b}]"; // ESC ]
const BEL: char = '\u{07}';
const ST: &str = "\u{1b}\\"; // ST = ESC \

/// The full payload prefix, namespace and key together. Case-sensitive: we generate
/// the emitter, so an off-case spelling is somebody else's sequence, not ours.
const KEY: &str = "1337;HyperpanesOpenURL=";

/// Longest URL we will carry out of a pane. Well under the OSC carry bound, and far
/// past any real link — a payload longer than this is someone probing, not a browser
/// request.
const MAX_URL: usize = 4096;

// Interpret one OSC payload (the bytes between `ESC]` and its terminator) as an
// open-URL request, applying the whole of the trust check: our key, a length a real
// link never exceeds, and a scheme the OS handler is safe to be handed.
#[tracing::instrument(level = "debug", ret)]
fn osc_data_to_url(data: &str) -> Option<String> {
    let url = data.strip_prefix(KEY)?;
    if url.len() > MAX_URL {
        return None;
    }
    // `is_openable_url` is the same gate the context-menu and click paths go through:
    // http/https/mailto only, no control characters, whitespace or quotes.
    if !crate::open::is_openable_url(url) {
        return None;
    }
    Some(url.to_string())
}

/// Pure, state-free scanner for open-URL OSC sequences. Given the `carry` from the
/// previous call and the next raw pty `chunk`, returns every URL the window contained,
/// in the order the pane emitted them, plus the carry to feed the next call. Handles
/// sequences split across chunks (split payload and split prefix) via a bounded carry.
///
/// A returned URL has already passed [`crate::open::is_openable_url`]; a caller still
/// owns the *policy* question of whether this pane may open a link at all.
#[tracing::instrument(level = "debug", ret)]
pub fn parse_osc_open_url(carry: &str, chunk: &str) -> (Vec<String>, String) {
    // Fast reject: nothing pending and no ESC anywhere → impossible to hold an OSC.
    if carry.is_empty() && !chunk.contains('\u{1b}') {
        return (Vec::new(), String::new());
    }

    let buf = format!("{carry}{chunk}");
    let mut urls: Vec<String> = Vec::new();
    let mut search_from = 0usize;
    while let Some(i) = buf[search_from..].find(OSC_PREFIX) {
        let start = i + search_from;
        let after_prefix = start + OSC_PREFIX.len();
        let bel_idx = buf[after_prefix..].find(BEL).map(|i| i + after_prefix);
        let st_idx = buf[after_prefix..].find(ST).map(|i| i + after_prefix);
        let (end, term_len) = match (bel_idx, st_idx) {
            (Some(b), st) if st.is_none_or(|s| b < s) => (b, 1),
            (_, Some(s)) => (s, ST.len()),
            _ => break, // incomplete sequence at the tail — handled by carry below
        };
        if let Some(url) = osc_data_to_url(&buf[after_prefix..end]) {
            urls.push(url);
        }
        search_from = end + term_len;
    }

    // Carry forward only a trailing partial that might complete in the next chunk.
    let mut next_carry = String::new();
    if let Some(last_start) = buf.rfind(OSC_PREFIX) {
        let after = last_start + OSC_PREFIX.len();
        let complete = buf[after..].contains(BEL) || buf[after..].contains(ST);
        if !complete {
            let tail = &buf[last_start..];
            // abandon oversized junk
            next_carry = if tail.len() > OSC_MAX {
                String::new()
            } else {
                tail.to_string()
            };
        }
    } else if buf.ends_with('\u{1b}') {
        // The 2-char prefix may be split: a lone trailing ESC starts the next OSC.
        next_carry = "\u{1b}".to_string();
    }

    (urls, next_carry)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BEL: &str = "\u{07}";
    const ESC: &str = "\u{1b}";

    fn seq(url: &str) -> String {
        format!("{ESC}]1337;HyperpanesOpenURL={url}{BEL}")
    }

    #[test]
    fn finds_a_plain_http_url_in_one_chunk() {
        let (urls, carry) = parse_osc_open_url("", &format!("log line{}", seq("http://a.test/x")));
        assert_eq!(urls, vec!["http://a.test/x"]);
        assert_eq!(carry, "");
    }

    #[test]
    fn returns_both_urls_when_two_arrive_in_one_chunk() {
        // An open is an event, not a state: dropping the first would lose a link the
        // user asked for.
        let (urls, _) = parse_osc_open_url(
            "",
            &format!(
                "{}noise{}",
                seq("https://one.test/"),
                seq("https://two.test/")
            ),
        );
        assert_eq!(urls, vec!["https://one.test/", "https://two.test/"]);
    }

    #[test]
    fn accepts_an_st_terminator() {
        let (urls, _) = parse_osc_open_url(
            "",
            &format!("{ESC}]1337;HyperpanesOpenURL=https://st.test/{ESC}\\"),
        );
        assert_eq!(urls, vec!["https://st.test/"]);
    }

    #[test]
    fn carries_a_payload_split_across_two_chunks() {
        let (urls_a, carry_a) =
            parse_osc_open_url("", &format!("{ESC}]1337;HyperpanesOpenURL=https://split."));
        assert!(urls_a.is_empty());
        let (urls_b, carry_b) = parse_osc_open_url(&carry_a, &format!("test/page{BEL}"));
        assert_eq!(urls_b, vec!["https://split.test/page"]);
        assert_eq!(carry_b, "");
    }

    #[test]
    fn carries_a_prefix_split_across_two_chunks() {
        let (_, carry_a) = parse_osc_open_url("", &format!("output{ESC}"));
        assert_eq!(carry_a, ESC);
        let (urls_b, _) = parse_osc_open_url(
            &carry_a,
            &format!("]1337;HyperpanesOpenURL=https://p.test/{BEL}"),
        );
        assert_eq!(urls_b, vec!["https://p.test/"]);
    }

    #[test]
    fn rejects_a_file_url() {
        // The sequence is well-formed; the scheme is not one the OS handler may be
        // handed from a pty.
        let (urls, _) = parse_osc_open_url("", &seq("file:///etc/passwd"));
        assert!(urls.is_empty());
    }

    #[test]
    fn rejects_a_javascript_url() {
        let (urls, _) = parse_osc_open_url("", &seq("javascript:alert(1)"));
        assert!(urls.is_empty());
    }

    #[test]
    fn rejects_an_over_long_payload() {
        let long = format!("https://long.test/{}", "a".repeat(MAX_URL));
        let (urls, _) = parse_osc_open_url("", &seq(&long));
        assert!(urls.is_empty());
    }

    #[test]
    fn ignores_another_1337_key_and_other_oscs() {
        let (urls, _) = parse_osc_open_url(
            "",
            &format!("{ESC}]1337;SetBadgeFormat=x{BEL}{ESC}]0;title{BEL}"),
        );
        assert!(urls.is_empty());
    }

    #[test]
    fn abandons_an_oversized_unterminated_sequence() {
        let huge = "x".repeat(20000);
        let (urls, carry) = parse_osc_open_url(
            "",
            &format!("{ESC}]1337;HyperpanesOpenURL=https://x.test/{huge}"),
        );
        assert!(urls.is_empty());
        assert_eq!(carry, "");
    }

    /// The exact bytes the generated shim wrote to `/dev/tty` when run under a real pty
    /// (`script -q /dev/null hp-open <two urls>`), leading terminal noise and all. The
    /// two halves of this feature only ever meet on a pty, so the wire format is pinned
    /// here rather than left to agreement between two hand-written constants.
    #[test]
    fn parses_a_real_pty_capture_from_the_shim() {
        let captured =
            "^D\u{8}\u{8}\u{1b}]1337;HyperpanesOpenURL=https://example.com/x?a=1&b=%20c\u{7}\
                        \u{1b}]1337;HyperpanesOpenURL=mailto:me@example.com\u{7}";
        let (urls, carry) = parse_osc_open_url("", captured);
        assert_eq!(
            urls,
            vec!["https://example.com/x?a=1&b=%20c", "mailto:me@example.com"]
        );
        assert_eq!(carry, "");
    }

    #[test]
    fn fast_rejects_a_plain_chunk_with_no_esc_and_no_carry() {
        let (urls, carry) = parse_osc_open_url("", "just some normal output\n");
        assert!(urls.is_empty());
        assert_eq!(carry, "");
    }
}

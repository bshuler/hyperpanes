//! Control-mode protocol tests.
//!
//! The interesting ones are **not** self-consistency checks — they are keyed to a transcript
//! captured from a real `tmux -CC` (3.7b) on this machine, so they fail if our wire format
//! drifts from tmux's rather than merely from our own past behaviour. The captures were
//! taken by running tmux under a pty and hexdumping the raw stream; the `\r` in the raw
//! bytes is the pty's ONLCR, not part of the protocol, so the fixtures below carry the LF
//! form the protocol actually specifies.

use super::*;

/// Frozen so `%begin`'s timestamp is comparable.
const T: u64 = 1_788_035_366;

fn srv() -> ControlServer {
    let mut a = PaneInfo::new("pane-aaaaaaaa-1111-4111-8111-111111111111");
    a.cols = Some(80);
    a.rows = Some(24);
    a.title = Some("zsh".into());
    ControlServer::new("cap", vec![a]).with_clock(Clock::Fixed(T))
}

fn two_pane() -> ControlServer {
    let mut a = PaneInfo::new("pane-aaaaaaaa-1111-4111-8111-111111111111");
    a.cols = Some(80);
    a.rows = Some(24);
    a.title = Some("zsh".into());
    let mut b = PaneInfo::new("pane-bbbbbbbb-2222-4222-8222-222222222222");
    b.cols = Some(100);
    b.rows = Some(30);
    b.title = Some("vim".into());
    ControlServer::new("cap", vec![a, b]).with_clock(Clock::Fixed(T))
}

/// Lines as UTF-8 strings, for the many assertions where the payload is ASCII.
fn text(lines: &[Line]) -> Vec<String> {
    lines
        .iter()
        .map(|l| String::from_utf8_lossy(l).into_owned())
        .collect()
}

// =====================================================================================
// %output escaping — the single most important thing in this module
// =====================================================================================

/// **Ground truth.** `tmux -CC` running
/// `printf '\177|\200|\377|\\|\001|\037|\176|\040|'` produced exactly these bytes after
/// `%output %0 ` (verbatim from `hexdump -C` of the captured stream):
///
/// ```text
/// 7f 7c 80 7c ff 7c 5c 31 33 34 7c 5c 30 30 31 7c 5c 30 33 37 7c 7e 7c 20 7c
/// ```
///
/// That is: **DEL raw, 0x80 raw, 0xFF raw**, `\` → `\134`, 0x01 → `\001`, 0x1F → `\037`,
/// `~` raw, space raw. Anyone who "fixes" [`escape_output`] to escape DEL or the high
/// bytes breaks iTerm2 on every non-ASCII pane; this test is the tripwire.
#[test]
fn escape_matches_real_tmux_capture() {
    let input: &[u8] = b"\x7f|\x80|\xff|\\|\x01|\x1f|~| |";
    let expected: &[u8] = b"\x7f|\x80|\xff|\\134|\\001|\\037|~| |";
    assert_eq!(escape_output(input), expected);
}

/// Second capture, from a pane echoing a UTF-8 line: `é` (c3 a9) and `€` (e2 82 ac) survive
/// byte-for-byte, and CRLF becomes `\015\012`.
#[test]
fn escape_matches_real_tmux_utf8_capture() {
    let input: &[u8] = b"\x80\xff\xc3\xa9\xe2\x82\xac !~\\\x01\x1f\r\n";
    let expected: &[u8] = b"\x80\xff\xc3\xa9\xe2\x82\xac !~\\134\\001\\037\\015\\012";
    assert_eq!(escape_output(input), expected);
}

/// A third real line: `printf "a\tb\\c\n"` came back as `a\011b\134c\015\012`.
#[test]
fn escape_matches_real_tmux_tab_capture() {
    assert_eq!(escape_output(b"a\tb\\c\r\n"), b"a\\011b\\134c\\015\\012");
}

/// The rule, stated exhaustively over the whole byte range, so no future edit can get one
/// boundary wrong without failing here: escape iff `b < 0x20 || b == b'\\'`.
#[test]
fn escape_is_exact_over_every_byte() {
    for b in 0u8..=255 {
        let got = escape_output(&[b]);
        if b < 0x20 || b == b'\\' {
            assert_eq!(
                got,
                format!("\\{:03o}", b).into_bytes(),
                "byte {b:#04x} should be octal-escaped"
            );
        } else {
            assert_eq!(got, vec![b], "byte {b:#04x} should pass through raw");
        }
    }
}

/// The boundaries, called out individually because they are where implementations slip:
/// 0x1F escapes, 0x20 does not; 0x7E, 0x7F and 0x80 are all raw.
#[test]
fn escape_boundaries() {
    assert_eq!(escape_output(&[0x1f]), b"\\037");
    assert_eq!(escape_output(&[0x20]), b" ");
    assert_eq!(escape_output(&[0x7e]), b"~");
    assert_eq!(escape_output(&[0x7f]), &[0x7fu8]);
    assert_eq!(escape_output(&[0x80]), &[0x80u8]);
    assert_eq!(escape_output(&[0x00]), b"\\000");
    assert_eq!(escape_output(&[0x5c]), b"\\134");
}

/// The reason [`escape_output`] returns `Vec<u8>`: an escaped payload is *not* valid UTF-8
/// in general, and building it as a `String` would silently re-encode every high byte.
#[test]
fn escaped_output_is_bytes_not_utf8() {
    let out = escape_output(&[0x80, 0xff]);
    assert_eq!(out, vec![0x80, 0xff]);
    assert!(String::from_utf8(out).is_err());
}

#[test]
fn escape_unescape_round_trips_every_byte() {
    let all: Vec<u8> = (0u8..=255).collect();
    assert_eq!(unescape_output(&escape_output(&all)), all);
}

#[test]
fn unescape_passes_through_malformed_escapes() {
    // Not three octal digits: keep the bytes rather than silently dropping them.
    assert_eq!(unescape_output(b"\\99"), b"\\99");
    // `\400` is 256, out of byte range, so it stays literal too.
    assert_eq!(unescape_output(b"\\4000"), b"\\4000");
    assert_eq!(unescape_output(b"a\\"), b"a\\");
}

// =====================================================================================
// Layout strings
// =====================================================================================

/// Both checksums are lifted straight out of the captured transcript:
/// `[layout b25d,80x24,0,0,0]` from `list-windows`, and
/// `%layout-change @0 a87d,100x30,0,0,0 …` after a resize to 100x30.
#[test]
fn layout_checksum_matches_real_tmux() {
    assert_eq!(format!("{:04x}", layout_checksum("80x24,0,0,0")), "b25d");
    assert_eq!(format!("{:04x}", layout_checksum("100x30,0,0,0")), "a87d");
}

#[test]
fn single_pane_layout_matches_real_tmux() {
    assert_eq!(single_pane_layout(80, 24, 0), "b25d,80x24,0,0,0");
    assert_eq!(single_pane_layout(100, 30, 0), "a87d,100x30,0,0,0");
}

#[test]
fn layout_checksum_is_not_a_plain_sum() {
    // Guards against the classic mis-port that drops the rotate: a plain byte sum would
    // make these two equal, since they are anagrams.
    assert_ne!(layout_checksum("80x24,0,0,1"), layout_checksum("80x24,0,1,0"));
}

// =====================================================================================
// Id mapping and its stability — clients cache these
// =====================================================================================

#[test]
fn ids_are_stable_across_reconnect_and_ordering() {
    let a = "pane-aaaaaaaa-1111-4111-8111-111111111111".to_string();
    let b = "pane-bbbbbbbb-2222-4222-8222-222222222222".to_string();
    let c = "pane-cccccccc-3333-4333-8333-333333333333".to_string();

    let forward = IdMap::rebuild(&[a.clone(), b.clone(), c.clone()]);
    let reverse = IdMap::rebuild(&[c.clone(), b.clone(), a.clone()]);
    // Order of discovery must not matter: a reconnect enumerates panes in whatever order
    // the daemon happens to answer in.
    assert_eq!(forward, reverse);

    // And a fresh process (which is what a reconnect really is) derives the same ids with
    // no persisted state at all.
    let reconnected = IdMap::rebuild(&[b.clone(), a.clone(), c.clone()]);
    assert_eq!(forward.pane_id(&a), reconnected.pane_id(&a));
    assert_eq!(forward.window_id(&c), reconnected.window_id(&c));
}

#[test]
fn ids_survive_an_unrelated_pane_disappearing() {
    let a = "pane-aaaaaaaa-1111-4111-8111-111111111111".to_string();
    let b = "pane-bbbbbbbb-2222-4222-8222-222222222222".to_string();
    let both = IdMap::rebuild(&[a.clone(), b.clone()]);
    let only_a = IdMap::rebuild(&[a.clone()]);
    // `b` closing must not renumber `a` — the client is still drawing `a`'s tab.
    assert_eq!(both.pane_id(&a), only_a.pane_id(&a));
    assert_eq!(both.window_id(&a), only_a.window_id(&a));
}

#[test]
fn ids_are_unique_and_fit_a_signed_int() {
    let uids: Vec<String> = (0..500).map(|i| format!("pane-{i:08x}-dead-4bee-8000-000000000000")).collect();
    let map = IdMap::rebuild(&uids);
    let mut seen = HashSet::new();
    for u in &uids {
        let p = map.pane_id(u).expect("every uid maps");
        assert!(p <= 0x7fff_ffff, "id must fit a signed 32-bit int");
        assert!(seen.insert(p), "pane ids must be unique");
    }
    assert_eq!(seen.len(), uids.len());
}

#[test]
fn id_reverse_lookup_round_trips() {
    let s = two_pane();
    for uid in ["pane-aaaaaaaa-1111-4111-8111-111111111111",
                "pane-bbbbbbbb-2222-4222-8222-222222222222"] {
        let p = s.ids().pane_id(uid).unwrap();
        let w = s.ids().window_id(uid).unwrap();
        assert_eq!(s.ids().uid_for_pane(p), Some(uid));
        assert_eq!(s.ids().uid_for_window(w), Some(uid));
    }
}

/// The id derivation is a documented contract, not an implementation detail: a client's
/// cache survives an upgrade only if these exact numbers keep coming out. Changing the hash
/// is a breaking change and must break this test.
#[test]
fn id_derivation_is_pinned() {
    let map = IdMap::rebuild(&["pane-aaaaaaaa-1111-4111-8111-111111111111".to_string()]);
    let uid = "pane-aaaaaaaa-1111-4111-8111-111111111111";
    assert_eq!(map.pane_id(uid), Some(hash31("pane", uid)));
    assert_eq!(map.window_id(uid), Some(hash31("window", uid)));
    // Pane and window namespaces are derived separately, so the two ids differ.
    assert_ne!(map.pane_id(uid), map.window_id(uid));
}

// =====================================================================================
// Greeting / goodbye framing — compared against the captured transcript
// =====================================================================================

/// The captured stream opened with, byte for byte:
/// `ESC P 1000 p`, `%begin <t> 279 0`, `%end <t> 279 0`, `%window-add @0`,
/// `%sessions-changed`, `%session-changed $0 c3`. Note the greeting block's **flags 0** —
/// it is tmux's own command, not the client's.
#[test]
fn greeting_matches_real_tmux_shape() {
    let mut s = srv();
    let g = s.greeting();
    let w = s.ids().window_id("pane-aaaaaaaa-1111-4111-8111-111111111111").unwrap();
    assert_eq!(g[0], DCS_OPEN);
    assert_eq!(
        text(&g[1..]),
        vec![
            format!("%begin {T} 0 0"),
            format!("%end {T} 0 0"),
            format!("%window-add @{w}"),
            "%sessions-changed".to_string(),
            "%session-changed $0 cap".to_string(),
        ]
    );
}

#[test]
fn greeting_emits_one_window_add_per_pane() {
    let mut s = two_pane();
    let g = text(&s.greeting());
    assert_eq!(g.iter().filter(|l| l.starts_with("%window-add ")).count(), 2);
}

#[test]
fn plain_mode_omits_the_dcs_wrapper() {
    let mut s = srv().with_mode(ControlMode::Plain);
    let g = s.greeting();
    assert_eq!(text(&g)[0], format!("%begin {T} 0 0"));
    assert_eq!(text(&s.goodbye(None)), vec!["%exit".to_string()]);
}

/// The capture ended `%exit\r\n` then `ESC \`.
#[test]
fn goodbye_matches_real_tmux() {
    let mut s = srv();
    let bye = s.goodbye(None);
    assert_eq!(text(&bye[..1]), vec!["%exit".to_string()]);
    assert_eq!(bye[1], DCS_CLOSE);

    let bye = s.goodbye(Some("server exited"));
    assert_eq!(text(&bye[..1]), vec!["%exit server exited".to_string()]);
}

// =====================================================================================
// Command block framing
// =====================================================================================

#[test]
fn command_is_wrapped_in_a_guard_block_with_flags_one() {
    let mut s = srv();
    let r = s.command("list-sessions");
    let lines = text(&r.lines);
    assert_eq!(lines.first().unwrap(), &format!("%begin {T} 1 1"));
    assert_eq!(lines.last().unwrap(), &format!("%end {T} 1 1"));
}

#[test]
fn block_numbers_increment_per_command() {
    let mut s = srv();
    for n in 1..=4 {
        let lines = text(&s.command("list-sessions").lines);
        assert_eq!(lines[0], format!("%begin {T} {n} 1"));
        assert_eq!(lines[lines.len() - 1], format!("%end {T} {n} 1"));
    }
}

/// tmux closes a failed command with `%error`, carrying the *same* timestamp and number as
/// its `%begin` — not with `%end`. A client that saw `%end` would believe the command
/// worked.
#[test]
fn error_block_closes_with_error_not_end() {
    let mut s = srv();
    let lines = text(&s.command("bogus-command-xyz").lines);
    assert_eq!(lines[0], format!("%begin {T} 1 1"));
    assert_eq!(lines[2], format!("%error {T} 1 1"));
    assert!(!lines.iter().any(|l| l.starts_with("%end")));
}

/// Verbatim from the capture: feeding real tmux `bogus-command-xyz` produced the body line
/// `parse error: unknown command: bogus-command-xyz`.
#[test]
fn unknown_command_body_matches_real_tmux() {
    let mut s = srv();
    let lines = text(&s.command("bogus-command-xyz").lines);
    assert_eq!(lines[1], "parse error: unknown command: bogus-command-xyz");
}

/// The plan is explicit that unimplemented commands must `%error` rather than silently
/// succeed. These are the destructive ones, where a silent `%end` would tell a client it
/// had killed a pane that is in fact still running someone's work.
#[test]
fn unsupported_lifecycle_commands_error_rather_than_lie() {
    for cmd in [
        "kill-pane -t %0",
        "kill-window -t @0",
        "kill-session",
        "kill-server",
        "new-window",
        "new-session -s x",
        "split-window -h",
        "rename-window -t @0 nope",
        "swap-pane -s %0 -t %1",
        "break-pane -t %0",
    ] {
        let mut s = srv();
        let lines = text(&s.command(cmd).lines);
        assert!(
            lines.iter().any(|l| l.starts_with("%error")),
            "{cmd} must be refused, got {lines:?}"
        );
    }
}

#[test]
fn a_quoting_error_is_reported_as_a_parse_error() {
    let mut s = srv();
    let lines = text(&s.command("send-keys -t '%0").lines);
    assert_eq!(lines[1], "parse error: unterminated single quote");
    assert!(lines[2].starts_with("%error"));
}

// =====================================================================================
// Notifications never land inside a block
// =====================================================================================

/// `control.c` defers notifications while a guard block is open and flushes them when the
/// outermost block closes — because everything between `%begin` and `%end` is the command's
/// *output*, and a stray `%window-pane-changed` in there corrupts the reply.
#[test]
fn notifications_are_flushed_after_the_block_not_inside_it() {
    let mut s = two_pane();
    let b = "pane-bbbbbbbb-2222-4222-8222-222222222222";
    let pid = s.ids().pane_id(b).unwrap();
    let lines = text(&s.command(&format!("select-pane -t %{pid}")).lines);

    let end = lines.iter().position(|l| l.starts_with("%end")).unwrap();
    let notes: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.starts_with("%session-window-changed") || l.starts_with("%window-pane-changed"))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(notes.len(), 2, "expected both notifications: {lines:?}");
    for i in notes {
        assert!(i > end, "notification at {i} landed inside the block: {lines:?}");
    }
}

#[test]
fn select_pane_changes_the_active_pane_and_flags() {
    let mut s = two_pane();
    let a = "pane-aaaaaaaa-1111-4111-8111-111111111111";
    let b = "pane-bbbbbbbb-2222-4222-8222-222222222222";
    let bpid = s.ids().pane_id(b).unwrap();
    assert_eq!(s.window_flags(a), "*");
    s.command(&format!("select-pane -t %{bpid}"));
    assert_eq!(s.window_flags(b), "*");
    assert_eq!(s.window_flags(a), "");
}

// =====================================================================================
// %output
// =====================================================================================

#[test]
fn output_line_is_prefixed_with_the_pane_id() {
    let mut s = srv();
    let uid = "pane-aaaaaaaa-1111-4111-8111-111111111111";
    let pid = s.ids().pane_id(uid).unwrap();
    let lines = s.output(uid, b"hi\n");
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0], format!("%output %{pid} hi\\012").into_bytes());
}

#[test]
fn output_for_an_unknown_pane_or_empty_chunk_emits_nothing() {
    let mut s = srv();
    assert!(s.output("pane-nope", b"hi").is_empty());
    assert!(s.output("pane-aaaaaaaa-1111-4111-8111-111111111111", b"").is_empty());
}

/// A `%output` line must never contain a raw newline, or the client would read the tail of
/// the pane's data as a protocol line. The escaping is what guarantees it.
#[test]
fn output_never_contains_a_raw_newline_or_cr() {
    let mut s = srv();
    let uid = "pane-aaaaaaaa-1111-4111-8111-111111111111";
    let payload: Vec<u8> = (0u8..=255).collect();
    let lines = s.output(uid, &payload);
    assert_eq!(lines.len(), 1);
    assert!(!lines[0].contains(&b'\n'));
    assert!(!lines[0].contains(&b'\r'));
}

// =====================================================================================
// list-* — default formats compared to the real transcript
// =====================================================================================

/// Real tmux printed `cap: 1 windows (created Sat Aug 29 16:27:42 2026) (attached)`. We
/// have no session creation time to report, so the parenthetical is dropped rather than
/// invented; everything else matches.
#[test]
fn list_sessions_default_shape() {
    let mut s = srv();
    let lines = text(&s.command("list-sessions").lines);
    assert_eq!(lines[1], "cap: 1 windows (attached)");
}

/// Verbatim from the capture:
/// `0: zsh* (1 panes) [80x24] [layout b25d,80x24,0,0,0] @0 (active)`.
/// Only the window id differs, because ours is derived from the durable uid.
#[test]
fn list_windows_default_matches_real_tmux() {
    let mut s = srv();
    let uid = "pane-aaaaaaaa-1111-4111-8111-111111111111";
    let w = s.ids().window_id(uid).unwrap();
    // The layout body embeds our real pane id, so the checksum differs from the capture's
    // `b25d` (which was computed over pane id 0). The checksum *algorithm* is pinned
    // against the real transcript in `layout_checksum_matches_real_tmux`; what this test
    // pins is the surrounding line shape.
    let layout = single_pane_layout(80, 24, s.ids().pane_id(uid).unwrap());
    let lines = text(&s.command("list-windows").lines);
    assert_eq!(
        lines[1],
        format!("0: zsh* (1 panes) [80x24] [layout {layout}] @{w} (active)")
    );
}

/// Verbatim from the capture:
/// `0: [80x24] [history 1/2000, 1800 bytes] %0 (active)`. Our history counters are always
/// zero (see the format table), which is the honest answer, not a guess.
#[test]
fn list_panes_default_matches_real_tmux() {
    let mut s = srv();
    let p = s.ids().pane_id("pane-aaaaaaaa-1111-4111-8111-111111111111").unwrap();
    let lines = text(&s.command("list-panes").lines);
    assert_eq!(
        lines[1],
        format!("0: [80x24] [history 0/2000, 0 bytes] %{p} (active)")
    );
}

/// iTerm2 never parses tmux's default output — it always passes `-F`. These are its real
/// format strings, taken from `TmuxController.m`.
#[test]
fn iterm2_format_strings_expand() {
    let mut s = two_pane();
    let a = "pane-aaaaaaaa-1111-4111-8111-111111111111";
    let b = "pane-bbbbbbbb-2222-4222-8222-222222222222";
    let (wa, pa) = (s.ids().window_id(a).unwrap(), s.ids().pane_id(a).unwrap());
    let (wb, pb) = (s.ids().window_id(b).unwrap(), s.ids().pane_id(b).unwrap());

    let lines = text(&s.command(r##"list-sessions -F "#{session_id} #{session_name}""##).lines);
    assert_eq!(lines[1], "$0 cap");

    let lines = text(
        &s.command(
            r##"list-windows -F "#{session_name}\t#{window_id}\t#{window_name}\t#{window_width}\t#{window_height}\t#{window_layout}\t#{?window_active,1,0}" -t $0"##,
        )
        .lines,
    );
    let (la, lb) = (single_pane_layout(80, 24, pa), single_pane_layout(100, 30, pb));
    assert_eq!(lines[1], format!("cap\t@{wa}\tzsh\t80\t24\t{la}\t1"));
    assert_eq!(lines[2], format!("cap\t@{wb}\tvim\t100\t30\t{lb}\t0"));

    let lines = text(&s.command(r##"list-panes -s -t $0 -F "#{pane_id}""##).lines);
    assert_eq!(lines[1], format!("%{pa}"));
    assert_eq!(lines[2], format!("%{pb}"));
}

#[test]
fn list_windows_narrows_on_a_window_target_but_not_a_session_target() {
    let mut s = two_pane();
    let wb = s.ids().window_id("pane-bbbbbbbb-2222-4222-8222-222222222222").unwrap();
    // `-t $0` is a session: every window.
    let lines = text(&s.command(r##"list-windows -F "#{window_id}" -t $0"##).lines);
    assert_eq!(lines.len(), 4, "{lines:?}");
    // `-t @n` is one window.
    let lines = text(&s.command(&format!(r##"list-windows -F "#{{window_id}}" -t @{wb}"##)).lines);
    assert_eq!(lines[1], format!("@{wb}"));
    assert_eq!(lines.len(), 3, "{lines:?}");
}

/// iTerm2's version probe. Claiming 3.2 selects the code paths we actually implement.
#[test]
fn display_message_reports_the_claimed_version() {
    let mut s = srv();
    let lines = text(&s.command(r##"display-message -p "#{version}""##).lines);
    assert_eq!(lines[1], CLAIMED_VERSION);
}

// =====================================================================================
// send-keys — every form a real client sends
// =====================================================================================

fn one_write(s: &mut ControlServer, cmd: &str) -> Vec<u8> {
    let r = s.command(cmd);
    assert!(
        text(&r.lines).iter().any(|l| l.starts_with("%end")),
        "{cmd} errored: {:?}",
        text(&r.lines)
    );
    match r.actions.as_slice() {
        [Action::Write { data, .. }] => data.clone(),
        other => panic!("{cmd} produced {other:?}"),
    }
}

/// `send -lt %n <chars>` — iTerm2's literal path. `-lt` is a *cluster*: boolean `-l`
/// followed by value-taking `-t`, which is exactly the case a naive flag parser gets wrong.
#[test]
fn send_keys_literal_cluster() {
    let mut s = srv();
    let p = s.ids().pane_id("pane-aaaaaaaa-1111-4111-8111-111111111111").unwrap();
    assert_eq!(one_write(&mut s, &format!("send -lt %{p} abc")), b"abc");
}

/// `send -t %n 0xNN` — iTerm2 sends *code points*, so they UTF-8 encode.
#[test]
fn send_keys_hex_code_points_are_utf8_encoded() {
    let mut s = srv();
    let p = s.ids().pane_id("pane-aaaaaaaa-1111-4111-8111-111111111111").unwrap();
    assert_eq!(one_write(&mut s, &format!("send -t %{p} 0x41 0x42")), b"AB");
    // U+20AC EURO SIGN -> three UTF-8 bytes, not one truncated one.
    assert_eq!(one_write(&mut s, &format!("send -t %{p} 0x20ac")), "€".as_bytes());
}

/// `send -H -t %n NN` — literal *bytes*, not code points. The distinction matters: the same
/// number means different things under `-H`.
#[test]
fn send_keys_hex_bytes_are_literal() {
    let mut s = srv();
    let p = s.ids().pane_id("pane-aaaaaaaa-1111-4111-8111-111111111111").unwrap();
    assert_eq!(
        one_write(&mut s, &format!("send -H -t %{p} 1b 5b 41")),
        b"\x1b[A"
    );
    // Contrast with the code-point path for the same digits.
    assert_eq!(one_write(&mut s, &format!("send -t %{p} 0xe9")), "é".as_bytes());
    assert_eq!(one_write(&mut s, &format!("send -H -t %{p} e9")), &[0xe9u8]);
}

/// Key names, which iTerm2 single-quotes (`send -t %n 'Enter'`).
#[test]
fn send_keys_named_keys() {
    let mut s = srv();
    let p = s.ids().pane_id("pane-aaaaaaaa-1111-4111-8111-111111111111").unwrap();
    for (name, want) in [
        ("Enter", &b"\r"[..]),
        ("Tab", b"\t"),
        ("Escape", b"\x1b"),
        ("BSpace", b"\x7f"),
        ("Up", b"\x1b[A"),
        ("Down", b"\x1b[B"),
        ("Right", b"\x1b[C"),
        ("Left", b"\x1b[D"),
        ("Home", b"\x1b[H"),
        ("End", b"\x1b[F"),
        ("PPage", b"\x1b[5~"),
        ("NPage", b"\x1b[6~"),
        ("DC", b"\x1b[3~"),
        ("IC", b"\x1b[2~"),
        ("F1", b"\x1bOP"),
        ("F5", b"\x1b[15~"),
        ("F12", b"\x1b[24~"),
        ("BTab", b"\x1b[Z"),
    ] {
        assert_eq!(
            one_write(&mut s, &format!("send -t %{p} '{name}'")),
            want,
            "key {name}"
        );
    }
}

#[test]
fn send_keys_modifiers() {
    let mut s = srv();
    let p = s.ids().pane_id("pane-aaaaaaaa-1111-4111-8111-111111111111").unwrap();
    assert_eq!(one_write(&mut s, &format!("send -t %{p} 'C-c'")), &[0x03]);
    assert_eq!(one_write(&mut s, &format!("send -t %{p} 'C-d'")), &[0x04]);
    assert_eq!(one_write(&mut s, &format!("send -t %{p} 'C-a'")), &[0x01]);
    assert_eq!(one_write(&mut s, &format!("send -t %{p} 'M-x'")), b"\x1bx");
    // Ctrl+Alt+A is ESC then 0x01 — both modifiers, not just the outermost one.
    assert_eq!(one_write(&mut s, &format!("send -t %{p} 'C-M-a'")), b"\x1b\x01");
    assert_eq!(one_write(&mut s, &format!("send -t %{p} 'M-C-a'")), b"\x1b\x01");
}

#[test]
fn send_keys_routes_to_the_targeted_pane() {
    let mut s = two_pane();
    let b = "pane-bbbbbbbb-2222-4222-8222-222222222222";
    let p = s.ids().pane_id(b).unwrap();
    let r = s.command(&format!("send -lt %{p} x"));
    assert_eq!(
        r.actions,
        vec![Action::Write { uid: b.to_string(), data: b"x".to_vec() }]
    );
}

#[test]
fn send_keys_accepts_a_window_target_too() {
    let mut s = two_pane();
    let b = "pane-bbbbbbbb-2222-4222-8222-222222222222";
    let w = s.ids().window_id(b).unwrap();
    let r = s.command(&format!("send -lt @{w} x"));
    assert_eq!(r.actions, vec![Action::Write { uid: b.to_string(), data: b"x".to_vec() }]);
}

#[test]
fn send_keys_with_a_bad_hex_byte_errors() {
    let mut s = srv();
    let p = s.ids().pane_id("pane-aaaaaaaa-1111-4111-8111-111111111111").unwrap();
    let r = s.command(&format!("send -H -t %{p} zz"));
    assert!(text(&r.lines).iter().any(|l| l.starts_with("%error")));
    assert!(r.actions.is_empty());
}

// =====================================================================================
// Resize policy
// =====================================================================================

/// Under the default `Observe` policy `refresh-client -C` is a *successful* no-op. It must
/// not `%error`: iTerm2 sends it unconditionally during attach, before it could know our
/// policy, and an error there aborts the attach.
#[test]
fn refresh_client_is_accepted_but_does_not_resize_under_observe() {
    let mut s = srv();
    for spec in ["refresh-client -C 100,30", "refresh-client -C 100x30"] {
        let r = s.command(spec);
        assert!(text(&r.lines).iter().any(|l| l.starts_with("%end")), "{spec}");
        assert!(r.actions.is_empty(), "{spec} must not resize under Observe");
    }
}

#[test]
fn refresh_client_resizes_under_request_policy() {
    let mut s = srv().with_policy(ResizePolicy::Request);
    let uid = "pane-aaaaaaaa-1111-4111-8111-111111111111".to_string();
    let r = s.command("refresh-client -C 100,30");
    assert_eq!(r.actions, vec![Action::Resize { uid, cols: 100, rows: 30 }]);
}

/// The per-window spelling, `-C @n:WxH`, which newer iTerm2 uses.
#[test]
fn refresh_client_per_window_form() {
    let mut s = two_pane().with_policy(ResizePolicy::Request);
    let b = "pane-bbbbbbbb-2222-4222-8222-222222222222";
    let w = s.ids().window_id(b).unwrap();
    let r = s.command(&format!("refresh-client -C @{w}:120x40"));
    assert_eq!(
        r.actions,
        vec![Action::Resize { uid: b.to_string(), cols: 120, rows: 40 }]
    );
}

/// `-f`, `-A` and `-B` configure flow control and subscriptions we do not implement.
/// Accepting and ignoring them is deliberate — see the command docs.
#[test]
fn refresh_client_ignores_flags_we_do_not_implement() {
    let mut s = srv();
    for cmd in [
        "refresh-client -f no-output",
        "refresh-client -A %0:on",
        "refresh-client -B 1:%0:#{pane_id}",
    ] {
        let r = s.command(cmd);
        assert!(text(&r.lines).iter().any(|l| l.starts_with("%end")), "{cmd}");
        assert!(r.actions.is_empty());
    }
}

#[test]
fn refresh_client_with_a_bad_size_errors() {
    let mut s = srv();
    let r = s.command("refresh-client -C nonsense");
    assert!(text(&r.lines).iter().any(|l| l.starts_with("%error")));
}

#[test]
fn resize_window_requests_a_resize_only_under_request_policy() {
    let uid = "pane-aaaaaaaa-1111-4111-8111-111111111111".to_string();
    let w = srv().ids().window_id(&uid).unwrap();

    let mut observe = srv();
    assert!(observe.command(&format!("resize-window -x 90 -y 25 -t @{w}")).actions.is_empty());

    let mut request = srv().with_policy(ResizePolicy::Request);
    let r = request.command(&format!("resize-window -x 90 -y 25 -t @{w}"));
    assert_eq!(r.actions, vec![Action::Resize { uid, cols: 90, rows: 25 }]);
}

// =====================================================================================
// Pane lifecycle notifications
// =====================================================================================

/// Verbatim from the capture, after resizing the client to 100x30:
/// `%layout-change @0 a87d,100x30,0,0,0 a87d,100x30,0,0,0 *`
/// — window id, layout, *visible* layout, then the raw window flags.
#[test]
fn pane_resize_emits_layout_change_matching_real_tmux() {
    let mut s = srv();
    let uid = "pane-aaaaaaaa-1111-4111-8111-111111111111";
    let w = s.ids().window_id(uid).unwrap();
    let layout = single_pane_layout(100, 30, s.ids().pane_id(uid).unwrap());
    let lines = text(&s.pane_resized(uid, 100, 30));
    assert_eq!(
        lines,
        vec![format!("%layout-change @{w} {layout} {layout} *")]
    );
}

#[test]
fn a_no_op_resize_emits_nothing() {
    let mut s = srv();
    assert!(s.pane_resized("pane-aaaaaaaa-1111-4111-8111-111111111111", 80, 24).is_empty());
}

#[test]
fn pane_added_emits_window_add_then_layout_change() {
    let mut s = srv();
    let new_uid = "pane-cccccccc-3333-4333-8333-333333333333";
    let mut info = PaneInfo::new(new_uid);
    info.cols = Some(80);
    info.rows = Some(24);
    let lines = text(&s.pane_added(info));
    let w = s.ids().window_id(new_uid).unwrap();
    assert_eq!(lines[0], format!("%window-add @{w}"));
    assert!(lines[1].starts_with(&format!("%layout-change @{w} ")));
}

#[test]
fn pane_exit_closes_the_window_but_keeps_the_session() {
    let mut s = two_pane();
    let b = "pane-bbbbbbbb-2222-4222-8222-222222222222";
    let w = s.ids().window_id(b).unwrap();
    let lines = text(&s.pane_exited(b));
    assert_eq!(lines, vec![format!("%window-close @{w}")]);
    // No %exit: the daemon and the other pane are still alive.
    assert!(!lines.iter().any(|l| l.starts_with("%exit")));
    // And the surviving pane keeps its ids.
    assert_eq!(
        s.ids().pane_id("pane-aaaaaaaa-1111-4111-8111-111111111111"),
        two_pane().ids().pane_id("pane-aaaaaaaa-1111-4111-8111-111111111111")
    );
}

#[test]
fn closing_the_active_pane_moves_active_to_a_survivor() {
    let mut s = two_pane();
    let a = "pane-aaaaaaaa-1111-4111-8111-111111111111";
    let b = "pane-bbbbbbbb-2222-4222-8222-222222222222";
    assert_eq!(s.window_flags(a), "*");
    s.pane_exited(a);
    assert_eq!(s.window_flags(b), "*");
}

#[test]
fn pane_rename_emits_window_renamed() {
    let mut s = srv();
    let uid = "pane-aaaaaaaa-1111-4111-8111-111111111111";
    let w = s.ids().window_id(uid).unwrap();
    assert_eq!(text(&s.pane_renamed(uid, "vim")), vec![format!("%window-renamed @{w} vim")]);
    // Idempotent.
    assert!(s.pane_renamed(uid, "vim").is_empty());
}

#[test]
fn adding_a_pane_that_already_exists_is_a_no_op() {
    let mut s = srv();
    assert!(s.pane_added(PaneInfo::new("pane-aaaaaaaa-1111-4111-8111-111111111111")).is_empty());
}

// =====================================================================================
// capture-pane
// =====================================================================================

#[test]
fn capture_pane_answers_from_the_screen_mirror() {
    let mut s = srv();
    let uid = "pane-aaaaaaaa-1111-4111-8111-111111111111";
    let p = s.ids().pane_id(uid).unwrap();
    s.set_screen(uid, Some("hello   \nworld   \n".to_string()));
    let lines = text(&s.command(&format!("capture-pane -p -t %{p}")).lines);
    assert_eq!(lines[1], "hello");
    assert_eq!(lines[2], "world");
    assert!(lines[3].starts_with("%end"));
}

/// `-P` asks for output not yet delivered. We stream everything as `%output` immediately,
/// so "nothing pending" is the truthful answer — and it is a success, not an error.
#[test]
fn capture_pane_pending_is_empty_and_successful() {
    let mut s = srv();
    let uid = "pane-aaaaaaaa-1111-4111-8111-111111111111";
    let p = s.ids().pane_id(uid).unwrap();
    s.set_screen(uid, Some("hello\n".to_string()));
    let lines = text(&s.command(&format!("capture-pane -p -P -C -t %{p}")).lines);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines[1].starts_with("%end"));
}

/// iTerm2's scrollback fetch clusters five booleans and passes a negative `-S`, which a
/// flag parser must not mistake for another flag.
#[test]
fn capture_pane_iterm2_scrollback_form_parses() {
    let mut s = srv();
    let uid = "pane-aaaaaaaa-1111-4111-8111-111111111111";
    let p = s.ids().pane_id(uid).unwrap();
    s.set_screen(uid, Some("line\n".to_string()));
    let lines = text(&s.command(&format!("capture-pane -peqJN -t \"%{p}\" -S -100")).lines);
    assert_eq!(lines[1], "line");
}

#[test]
fn wants_screen_refresh_only_fires_for_capture_pane() {
    assert!(wants_screen_refresh("capture-pane -p -t %0"));
    assert!(wants_screen_refresh("  capturep -p"));
    assert!(!wants_screen_refresh("list-panes"));
    assert!(!wants_screen_refresh("send -lt %0 capture-pane"));
}

// =====================================================================================
// Detach
// =====================================================================================

/// `control.c:control_read_callback` treats an empty line as a detach.
#[test]
fn an_empty_line_detaches() {
    for line in ["", "\n", "\r\n", "   "] {
        let mut s = srv();
        let r = s.command(line);
        assert_eq!(r.actions, vec![Action::Detach], "for {line:?}");
        assert!(r.lines.is_empty(), "detach must not emit a block");
    }
}

#[test]
fn detach_command_detaches() {
    let mut s = srv();
    for cmd in ["detach", "detach-client", "detach-client -P"] {
        let r = s.command(cmd);
        assert!(r.actions.contains(&Action::Detach), "{cmd}");
    }
}

/// A client behind a pty or an SSH channel may deliver CRLF; the protocol splits on LF.
#[test]
fn a_trailing_cr_is_tolerated() {
    let mut s = srv();
    let lines = text(&s.command("list-sessions\r\n").lines);
    assert_eq!(lines[1], "cap: 1 windows (attached)");
}

// =====================================================================================
// Lexing and flag parsing
// =====================================================================================

#[test]
fn split_words_handles_tmux_quoting() {
    assert_eq!(split_words("a b  c").unwrap(), vec!["a", "b", "c"]);
    // Single quotes are fully literal — which is why iTerm2 uses them for key names.
    assert_eq!(split_words("send -t %0 'C-c'").unwrap(), vec!["send", "-t", "%0", "C-c"]);
    assert_eq!(split_words(r##"a "b c" d"##).unwrap(), vec!["a", "b c", "d"]);
    assert_eq!(split_words(r##""a\tb""##).unwrap(), vec!["a\tb"]);
    assert_eq!(split_words(r##"a\ b"##).unwrap(), vec!["a b"]);
    assert_eq!(split_words(r##"'#{pane_id}'"##).unwrap(), vec!["#{pane_id}"]);
    assert!(split_words("'unterminated").is_err());
    assert!(split_words(r##""unterminated"##).is_err());
    assert!(split_words("trailing\\").is_err());
}

#[test]
fn args_parse_clusters_and_values() {
    let w: Vec<String> = split_words("-lt %0 abc").unwrap();
    let a = Args::parse(&w, "t:").unwrap();
    assert!(a.flag('l'));
    assert_eq!(a.value('t'), Some("%0"));
    assert_eq!(a.positionals, vec!["abc"]);

    // Attached value: `-tFOO`.
    let w: Vec<String> = split_words("-t%0").unwrap();
    assert_eq!(Args::parse(&w, "t:").unwrap().value('t'), Some("%0"));

    // A negative number is a value, not a flag.
    let w: Vec<String> = split_words("-S -100").unwrap();
    assert_eq!(Args::parse(&w, "S:").unwrap().value('S'), Some("-100"));

    // A value-taking flag with nothing after it is an error, not a silent empty value.
    let w: Vec<String> = split_words("-t").unwrap();
    assert!(Args::parse(&w, "t:").is_err());
}

// =====================================================================================
// Format expansion
// =====================================================================================

#[test]
fn format_conditionals_and_escapes() {
    let s = two_pane();
    let a = FmtCtx { uid: Some("pane-aaaaaaaa-1111-4111-8111-111111111111".into()) };
    let b = FmtCtx { uid: Some("pane-bbbbbbbb-2222-4222-8222-222222222222".into()) };
    assert_eq!(s.expand("#{?window_active,yes,no}", &a), "yes");
    assert_eq!(s.expand("#{?window_active,yes,no}", &b), "no");
    assert_eq!(s.expand("#{?window_active,1,0}", &b), "0");
    // `##` is a literal '#'.
    assert_eq!(s.expand("a##b", &a), "a#b");
    // An unknown variable expands to nothing, as tmux does — an unrecognised probe must
    // degrade quietly rather than break the attach.
    assert_eq!(s.expand("[#{no_such_variable}]", &a), "[]");
    // Modifier prefixes are stripped and the inner template expanded.
    assert_eq!(s.expand("#{E:#{window_name}}", &a), "zsh");
    assert_eq!(s.expand("#{T:#{window_name}}", &a), "zsh");
    // A stray '#' and an unbalanced brace must not panic or loop.
    assert_eq!(s.expand("100% #", &a), "100% #");
    assert_eq!(s.expand("#{unclosed", &a), "#{unclosed");
}

/// The expander walks bytes; copying them one at a time as `char` would turn any non-ASCII
/// into Latin-1 mojibake. A cwd is the realistic source of one.
#[test]
fn format_expansion_preserves_non_ascii() {
    let mut p = PaneInfo::new("pane-aaaaaaaa-1111-4111-8111-111111111111");
    p.cwd = Some("/Users/bshuler/Ünicode/日本語".into());
    let s = ControlServer::new("cap", vec![p]).with_clock(Clock::Fixed(T));
    let ctx = FmtCtx { uid: Some("pane-aaaaaaaa-1111-4111-8111-111111111111".into()) };
    assert_eq!(s.expand("→#{pane_current_path}←", &ctx), "→/Users/bshuler/Ünicode/日本語←");
}

#[test]
fn unknown_grid_falls_back_to_eighty_by_twenty_four() {
    let mut s = ControlServer::new("cap", vec![PaneInfo::new("pane-x")]).with_clock(Clock::Fixed(T));
    let lines = text(&s.command(r##"list-windows -F "#{window_width}x#{window_height}""##).lines);
    assert_eq!(lines[1], "80x24");
}

// =====================================================================================
// A whole attach, end to end
// =====================================================================================

/// The full sequence a control client drives on connect, asserted as one golden transcript.
/// This is the test that would catch an ordering regression — a `%window-add` after the
/// `%session-changed`, a notification inside a block, a missing guard — that the
/// finer-grained tests each miss individually.
#[test]
fn full_attach_transcript() {
    let mut s = two_pane();
    let a = "pane-aaaaaaaa-1111-4111-8111-111111111111";
    let b = "pane-bbbbbbbb-2222-4222-8222-222222222222";
    let (wa, pa) = (s.ids().window_id(a).unwrap(), s.ids().pane_id(a).unwrap());
    let (wb, pb) = (s.ids().window_id(b).unwrap(), s.ids().pane_id(b).unwrap());

    let mut got: Vec<String> = Vec::new();
    got.extend(text(&s.greeting()));
    // Replay seeding: the daemon's rolling buffer, delivered as %output.
    got.extend(text(&s.output(a, b"$ ")));
    for cmd in [
        r##"list-sessions -F "#{session_id} #{session_name}""##,
        r##"list-windows -F "#{window_id} #{window_name} #{window_layout}" -t $0"##,
        r##"list-panes -s -t $0 -F "#{pane_id} #{window_id}""##,
        "refresh-client -C 80,24",
    ] {
        got.extend(text(&s.command(cmd).lines));
    }
    got.extend(text(&s.output(b, b"\x1b[1mbold\x1b[0m\r\n")));
    got.extend(text(&s.pane_exited(b)));
    got.extend(text(&s.goodbye(None)));

    let want = vec![
        String::from_utf8_lossy(DCS_OPEN).into_owned(),
        format!("%begin {T} 0 0"),
        format!("%end {T} 0 0"),
        format!("%window-add @{wa}"),
        format!("%window-add @{wb}"),
        "%sessions-changed".into(),
        "%session-changed $0 cap".into(),
        format!("%output %{pa} $ "),
        format!("%begin {T} 1 1"),
        "$0 cap".into(),
        format!("%end {T} 1 1"),
        format!("%begin {T} 2 1"),
        format!("@{wa} zsh {}", single_pane_layout(80, 24, pa)),
        format!("@{wb} vim {}", single_pane_layout(100, 30, pb)),
        format!("%end {T} 2 1"),
        format!("%begin {T} 3 1"),
        format!("%{pa} @{wa}"),
        format!("%{pb} @{wb}"),
        format!("%end {T} 3 1"),
        format!("%begin {T} 4 1"),
        format!("%end {T} 4 1"),
        format!("%output %{pb} \\033[1mbold\\033[0m\\015\\012"),
        format!("%window-close @{wb}"),
        "%exit".into(),
        String::from_utf8_lossy(DCS_CLOSE).into_owned(),
    ];
    assert_eq!(got, want);
}

/// Reconnecting to the same panes must hand the client the same ids it cached — otherwise
/// every reattach orphans its tabs. This exercises the whole path, not just [`IdMap`].
#[test]
fn a_reconnect_reproduces_the_same_ids() {
    let first = {
        let mut s = two_pane();
        text(&s.greeting())
    };
    // A second, independent server built from the same panes in the opposite order, as a
    // fresh process after a reconnect would be.
    let second = {
        let mut b = PaneInfo::new("pane-bbbbbbbb-2222-4222-8222-222222222222");
        b.cols = Some(100);
        b.rows = Some(30);
        let mut a = PaneInfo::new("pane-aaaaaaaa-1111-4111-8111-111111111111");
        a.cols = Some(80);
        a.rows = Some(24);
        let mut s = ControlServer::new("cap", vec![b, a]).with_clock(Clock::Fixed(T));
        text(&s.greeting())
    };
    assert_eq!(first, second);
}

// ---------------------------------------------------------------------------
// Wire framing: which lines take a newline
// ---------------------------------------------------------------------------

#[test]
fn only_the_dcs_wrapper_skips_the_newline() {
    // Ground truth from the live capture: `\x1bP1000p` runs straight into `%begin` with no
    // separator, and the closing `\x1b\\` is the last byte pair of the stream after
    // `%exit\n`. Everything else is one line + "\n" (control.c:control_write_line).
    assert!(!needs_newline(DCS_OPEN));
    assert!(!needs_newline(DCS_CLOSE));
    assert!(needs_newline(b"%begin 1 1 1"));
    assert!(needs_newline(b"%exit"));
    assert!(needs_newline(b"%output %0 hi"));
    // Not a prefix test: a line that merely starts with ESC still terminates.
    assert!(needs_newline(b"\x1bP1000p%begin 1 1 1"));
}

#[test]
fn the_full_greeting_serializes_to_the_captured_byte_stream() {
    let mut s = srv();
    let mut wire = Vec::new();
    for line in s.greeting() {
        wire.extend_from_slice(&line);
        if needs_newline(&line) {
            wire.push(b'\n');
        }
    }
    let head = format!(
        "\x1bP1000p%begin {T} 0 0\n%end {T} 0 0\n%window-add @{}\n%sessions-changed\n\
         %session-changed $0 cap\n",
        s.ids()
            .window_id("pane-aaaaaaaa-1111-4111-8111-111111111111")
            .unwrap()
    );
    assert_eq!(String::from_utf8(wire).unwrap(), head);
}

#[test]
fn goodbye_serializes_with_the_st_unterminated() {
    let mut s = srv();
    let mut wire = Vec::new();
    for line in s.goodbye(None) {
        wire.extend_from_slice(&line);
        if needs_newline(&line) {
            wire.push(b'\n');
        }
    }
    assert_eq!(wire, b"%exit\n\x1b\\".to_vec());
}

// ---------------------------------------------------------------------------
// Driver-facing state setters
// ---------------------------------------------------------------------------

#[test]
fn set_cwd_feeds_the_pane_current_path_format() {
    let uid = "pane-aaaaaaaa-1111-4111-8111-111111111111";
    let mut s = srv();
    s.set_cwd(uid, Some("/tmp/wörk".into()));
    let pane = s.ids().pane_id(uid).unwrap();
    let r = s.command(&format!("display-message -p -t %{pane} '#{{pane_current_path}}'"));
    assert_eq!(text(&r.lines)[1], "/tmp/wörk");
    // An unknown uid is a silent no-op, not a panic — the daemon can report a pane this
    // client has not learned about yet.
    s.set_cwd("pane-nope", Some("/x".into()));
}

#[test]
fn has_pane_and_uids_track_the_published_set() {
    let mut s = srv();
    let a = "pane-aaaaaaaa-1111-4111-8111-111111111111";
    let b = "pane-bbbbbbbb-2222-4222-8222-222222222222";
    assert!(s.has_pane(a));
    assert!(!s.has_pane(b));
    assert_eq!(s.uids(), vec![a.to_string()]);
    s.pane_added(PaneInfo::new(b));
    assert_eq!(s.uids(), vec![a.to_string(), b.to_string()]);
    s.pane_exited(a);
    assert!(!s.has_pane(a));
    assert_eq!(s.uids(), vec![b.to_string()]);
}

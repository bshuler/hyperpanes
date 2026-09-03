//! Text-to-speech normalization: strip whatever markup the text arrived wearing so a TTS
//! backend reads prose instead of punctuation.
//!
//! The input is "whatever a tool wrote down", and tools do not agree on a format. A reply
//! about HTML contains HTML; a tool that reports structurally answers in JSON; a table of
//! numbers turns up as CSV; terminal output carries ANSI colour and spinner frames; and
//! Markdown, the original and still the common case, carries all of the above inside
//! itself. A synthesizer reads every one of those literally, so each has to be recognized
//! and reduced here or it is heard as noise.
//!
//! The pipeline is: strip escapes and drawing characters (they can appear in any format),
//! then try the two whole-document formats whose punctuation *is* their structure (JSON,
//! CSV) before anything is stripped from them, then treat what is left as Markdown that
//! may contain HTML — which is exactly what an assistant reply is. See
//! [`super::markup`] for the format-specific halves.
//!
//! Every guess in here is built to fail soft. Detection is deliberately conservative and
//! the transformations are close to identity on text that is really just prose, because
//! the cost of being wrong is a mangled sentence that nobody can review — the listener
//! hears the output, and the input is already gone.

use super::markup;

/// Hard cap on a spoken utterance's length, in `char`s.
const MAX_LEN: usize = 1200;
const TRUNCATED_SUFFIX: &str = " Truncated.";

/// Reduce an assistant reply — in any text format — to speakable plain text.
///
/// - ANSI escape sequences, box drawing, spinner frames and zero-width characters go first
/// - a whole document of JSON or CSV is flattened to its readable leaves
/// - fenced code becomes "code block omitted."; HTML becomes the words between its tags
/// - Markdown markup (headers, lists, quotes, tables, links, emphasis) is removed, keeping
///   the text it decorated
/// - whitespace collapses to single spaces, and the result is capped at [`MAX_LEN`] chars
///   at the last sentence boundary before the cap, with `" Truncated."` appended
#[tracing::instrument(level = "debug", ret)]
pub fn normalize_for_speech(input: &str) -> String {
    let text = markup::strip_ansi(input);
    let text = markup::strip_decoration(&text);

    // These two are tried on the raw document because their punctuation is their meaning:
    // strip a JSON document's braces as if they were markup and there is nothing left to
    // tell a key from a value.
    if let Some(spoken) = markup::json_to_text(&text) {
        return finish(&spoken);
    }
    if !looks_like_markdown(&text) {
        if let Some(spoken) = markup::csv_to_text(&text) {
            return finish(&spoken);
        }
    }

    let text = strip_code_fences(&text);
    let text = if markup::looks_like_html(&text) {
        markup::html_to_text(&text)
    } else {
        text
    };
    finish(&markdown_to_prose(&text))
}

/// Veto on the CSV guess: anything wearing Markdown's clothes is Markdown.
///
/// Two lines of prose with a comma each ("Ran the tests, they passed.") do parse as a
/// two-column table, and flattening one is very nearly the identity — which is why the
/// CSV guess is safe at all. But a Markdown *list* of such lines is not: reading it as a
/// table would skip the list handling below and leave the emphasis markers in.
#[tracing::instrument(level = "debug", ret)]
fn looks_like_markdown(text: &str) -> bool {
    text.contains("**")
        || text.contains('`')
        || text.lines().any(|line| {
            let t = line.trim_start();
            t.starts_with('#')
                || t.starts_with("- ")
                || t.starts_with("* ")
                || t.starts_with("> ")
                || t.starts_with('|')
        })
}

/// Drop leftover markup characters, collapse whitespace, and cap the length.
///
/// The character filter is belt-and-suspenders for anything the structured passes missed —
/// a stray emphasis marker, an unpaired bracket — which would otherwise be read out as a
/// word.
#[tracing::instrument(level = "debug", ret)]
fn finish(text: &str) -> String {
    let stripped: String = text
        .chars()
        .filter(|c| !matches!(c, '`' | '*' | '#' | '[' | ']'))
        .collect();
    truncate_at_sentence(&collapse_whitespace(&stripped), MAX_LEN)
}

/// Remove Markdown markup, keeping the text it decorated.
#[tracing::instrument(level = "debug", ret)]
fn markdown_to_prose(input: &str) -> String {
    let lines: Vec<String> = input
        .lines()
        .map(|line| {
            // A rule, a setext underline and a link definition are all pure markup: there
            // is no text under them to keep.
            if is_rule_line(line) || is_link_definition(line) {
                return String::new();
            }
            let line = strip_blockquote(line);
            let line = strip_atx_header(&line);
            let line = strip_list_marker(&line);
            let line = strip_task_box(&line);
            convert_table_row_or_pass(&line)
        })
        .collect();
    let joined = lines.join("\n");
    // An image's alt text is text an author wrote for someone who cannot see the image,
    // which describes a listener exactly. Dropping the `!` hands it to the link stripper.
    let joined = joined.replace("![", "[");
    let joined = strip_markdown_links(&joined);
    let joined = strip_reference_links(&joined);
    let joined = strip_autolinks(&joined);
    let joined = strip_bare_urls(&joined);
    joined.replace("~~", "")
}

/// Replace each fenced code block (``` ... ```) with the phrase "code block omitted." —
/// hearing raw code read character-by-character is noise, not information; the listener
/// can look at the pane for the code itself.
#[tracing::instrument(level = "debug", ret)]
fn strip_code_fences(input: &str) -> String {
    let mut out_lines = Vec::new();
    let mut in_fence = false;
    for line in input.lines() {
        if line.trim_start().starts_with("```") {
            if !in_fence {
                out_lines.push("code block omitted.");
            }
            in_fence = !in_fence;
            continue;
        }
        if !in_fence {
            out_lines.push(line);
        }
    }
    out_lines.join("\n")
}

/// Is this line a thematic break (`---`, `***`, `___`) or a setext heading underline
/// (`===`, `---`)? Both are runs of one character, and neither says anything.
#[tracing::instrument(level = "debug", ret)]
fn is_rule_line(line: &str) -> bool {
    let trimmed = line.trim();
    let Some(first) = trimmed.chars().next() else {
        return false;
    };
    if !matches!(first, '-' | '=' | '*' | '_') {
        return false;
    }
    let marks = trimmed.chars().filter(|&c| c == first).count();
    marks >= 2 && trimmed.chars().all(|c| c == first || c == ' ')
}

/// Is this line a link reference definition (`[label]: https://…`)? The label is repaid
/// where it is used; the line itself is bookkeeping.
#[tracing::instrument(level = "debug", ret)]
fn is_link_definition(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with('[')
        && trimmed
            .find("]:")
            .is_some_and(|close| !trimmed[1..close].contains('['))
}

/// Strip blockquote markers (`> `, and `>> ` for a quote inside a quote), keeping the
/// quoted text.
#[tracing::instrument(level = "debug", ret)]
fn strip_blockquote(line: &str) -> String {
    let mut rest = line.trim_start();
    let mut stripped = false;
    while let Some(next) = rest.strip_prefix('>') {
        rest = next.strip_prefix(' ').unwrap_or(next);
        stripped = true;
    }
    if stripped {
        rest.to_string()
    } else {
        line.to_string()
    }
}

/// Strip a leading list marker (`- `, `* `, `+ `, or `1. `-style) from one line,
/// keeping the item text.
#[tracing::instrument(level = "debug", ret)]
fn strip_list_marker(line: &str) -> String {
    let trimmed = line.trim_start();
    for m in ["- ", "* ", "+ "] {
        if let Some(rest) = trimmed.strip_prefix(m) {
            return rest.to_string();
        }
    }
    let digits = trimmed.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0 {
        if let Some(rest) = trimmed[digits..].strip_prefix(". ") {
            return rest.to_string();
        }
    }
    line.to_string()
}

/// Strip a task-list checkbox (`[ ]`, `[x]`) from the front of an item.
///
/// It is dropped rather than spoken as "done" or "to do": the marker is a glyph in a UI,
/// and inventing a word for it puts a claim in the listener's ear that the author did not
/// write.
#[tracing::instrument(level = "debug", ret)]
fn strip_task_box(line: &str) -> String {
    let trimmed = line.trim_start();
    for box_ in ["[ ] ", "[x] ", "[X] ", "[ ]", "[x]", "[X]"] {
        if let Some(rest) = trimmed.strip_prefix(box_) {
            return rest.to_string();
        }
    }
    line.to_string()
}

/// Strip a leading ATX header marker (`#` through `######` followed by a space) from
/// one line, leaving the heading text.
#[tracing::instrument(level = "debug", ret)]
fn strip_atx_header(line: &str) -> String {
    let trimmed = line.trim_start();
    let hashes = trimmed.chars().take_while(|&c| c == '#').count();
    if (1..=6).contains(&hashes) {
        let rest = &trimmed[hashes..];
        return rest.strip_prefix(' ').unwrap_or(rest).to_string();
    }
    line.to_string()
}

#[tracing::instrument(level = "debug", ret)]
fn is_table_separator_row(trimmed: &str) -> bool {
    !trimmed.is_empty()
        && trimmed.contains('-')
        && trimmed
            .chars()
            .all(|c| matches!(c, '|' | '-' | ':' | ' ' | '\t'))
}

/// If `line` looks like a `|`-delimited table row, flatten it to `", "`-joined cells
/// (dropping a pure separator row entirely); otherwise return it unchanged.
#[tracing::instrument(level = "debug", ret)]
fn convert_table_row_or_pass(line: &str) -> String {
    let trimmed = line.trim();
    if !trimmed.contains('|') {
        return line.to_string();
    }
    if is_table_separator_row(trimmed) {
        return String::new();
    }
    let inner = trimmed.trim_start_matches('|').trim_end_matches('|');
    inner
        .split('|')
        .map(str::trim)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Replace `[text](url)` with `text`. Non-nested, single-line matches only.
#[tracing::instrument(level = "debug", ret)]
fn strip_markdown_links(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '[' {
            if let Some(close_rel) = chars[i + 1..].iter().position(|&c| c == ']') {
                let text_end = i + 1 + close_rel;
                let paren_start = text_end + 1;
                if chars.get(paren_start) == Some(&'(') {
                    if let Some(close_paren_rel) =
                        chars[paren_start + 1..].iter().position(|&c| c == ')')
                    {
                        let url_end = paren_start + 1 + close_paren_rel;
                        let text: String = chars[i + 1..text_end].iter().collect();
                        out.push_str(&text);
                        i = url_end + 1;
                        continue;
                    }
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Replace a reference link (`[text][label]`, or `[text][]`) with `text`. The label points
/// at a definition line that [`is_link_definition`] has already dropped.
#[tracing::instrument(level = "debug", ret)]
fn strip_reference_links(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '[' {
            if let Some(close_rel) = chars[i + 1..].iter().position(|&c| c == ']') {
                let text_end = i + 1 + close_rel;
                if chars.get(text_end + 1) == Some(&'[') {
                    if let Some(label_end_rel) =
                        chars[text_end + 2..].iter().position(|&c| c == ']')
                    {
                        let text: String = chars[i + 1..text_end].iter().collect();
                        out.push_str(&text);
                        i = text_end + 2 + label_end_rel + 1;
                        continue;
                    }
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Remove Markdown autolinks (`<https://…>`, `<mailto:…>`), which [`strip_bare_urls`]
/// cannot see because of the brackets around them.
#[tracing::instrument(level = "debug", ret)]
fn strip_autolinks(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(open) = rest.find('<') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let is_link = ["http://", "https://", "mailto:", "ftp://"]
            .iter()
            .any(|p| after.starts_with(p));
        match after.find('>').filter(|_| is_link) {
            Some(close) => rest = &after[close + 1..],
            None => {
                out.push('<');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Remove bare `http://`/`https://` URLs (anything up to the next whitespace).
#[tracing::instrument(level = "debug", ret)]
fn strip_bare_urls(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while !rest.is_empty() {
        if rest.starts_with("http://") || rest.starts_with("https://") {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            rest = &rest[end..];
            continue;
        }
        let ch = rest.chars().next().expect("rest is non-empty");
        out.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
    out
}

/// Collapse every run of whitespace (including newlines) to a single space, and trim
/// the ends.
#[tracing::instrument(level = "debug", ret)]
fn collapse_whitespace(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut last_was_space = false;
    for ch in input.chars() {
        if ch.is_whitespace() {
            if !last_was_space {
                out.push(' ');
            }
            last_was_space = true;
        } else {
            out.push(ch);
            last_was_space = false;
        }
    }
    out.trim().to_string()
}

/// Cap `input` at `max` chars, cutting at the last sentence-ending punctuation before
/// the cap (inclusive) and appending [`TRUNCATED_SUFFIX`]. If no sentence boundary is
/// found, hard-cuts at `max` chars instead.
#[tracing::instrument(level = "debug", ret)]
fn truncate_at_sentence(input: &str, max: usize) -> String {
    if input.chars().count() <= max {
        return input.to_string();
    }
    let truncated: String = input.chars().take(max).collect();
    let cut = truncated.rfind(['.', '!', '?']).map(|idx| idx + 1);
    let base = match cut {
        Some(idx) => &truncated[..idx],
        None => truncated.as_str(),
    };
    format!("{}{TRUNCATED_SUFFIX}", base.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- plain text ----------------------------------------------------------

    #[test]
    fn plain_prose_survives_unchanged() {
        let input = "I fixed the parser. It now handles the empty case, and the tests pass.";
        assert_eq!(normalize_for_speech(input), input);
    }

    #[test]
    fn identifiers_with_underscores_and_tildes_are_not_mangled() {
        let input = "Check exit_code in ~/.config and call do_the_thing twice.";
        assert_eq!(normalize_for_speech(input), input);
    }

    // ---- markdown ------------------------------------------------------------

    #[test]
    fn headers_lose_their_hashes() {
        assert_eq!(normalize_for_speech("## What changed"), "What changed");
    }

    #[test]
    fn list_markers_are_dropped_but_items_are_kept() {
        let out = normalize_for_speech("- first\n* second\n+ third\n1. fourth");
        assert_eq!(out, "first second third fourth");
    }

    #[test]
    fn task_list_boxes_are_dropped_without_inventing_a_word() {
        let out = normalize_for_speech("- [x] shipped\n- [ ] pending");
        assert_eq!(out, "shipped pending");
        assert!(!out.contains("done") && !out.contains("to do"));
    }

    #[test]
    fn blockquotes_are_read_as_their_text() {
        assert_eq!(normalize_for_speech("> quoted line"), "quoted line");
    }

    #[test]
    fn rules_and_setext_underlines_say_nothing() {
        assert_eq!(
            normalize_for_speech("Title\n=====\n\nBody.\n\n---\n"),
            "Title Body."
        );
    }

    #[test]
    fn tables_become_comma_separated_cells() {
        let table = "| name | count |\n| --- | --- |\n| alpha | 3 |";
        assert_eq!(normalize_for_speech(table), "name, count alpha, 3");
    }

    #[test]
    fn fenced_code_is_replaced_by_a_phrase() {
        let input = "Here:\n\n```rust\nfn main() {}\n```\n\nDone.";
        assert_eq!(
            normalize_for_speech(input),
            "Here: code block omitted. Done."
        );
    }

    #[test]
    fn links_keep_their_text_and_lose_their_url() {
        let out = normalize_for_speech("See [the docs](https://example.com/x) for more.");
        assert_eq!(out, "See the docs for more.");
    }

    #[test]
    fn reference_links_and_their_definitions_are_resolved() {
        let out = normalize_for_speech("See [the docs][d] for more.\n\n[d]: https://example.com/x");
        assert_eq!(out, "See the docs for more.");
    }

    #[test]
    fn autolinks_and_bare_urls_are_dropped() {
        let out = normalize_for_speech("Try <https://example.com/a> or https://example.com/b now.");
        assert_eq!(out, "Try or now.");
    }

    #[test]
    fn image_alt_text_is_spoken() {
        let out = normalize_for_speech("![a red bicycle](bike.png)");
        assert_eq!(out, "a red bicycle");
    }

    #[test]
    fn strikethrough_markers_are_dropped_but_the_text_stays() {
        assert_eq!(
            normalize_for_speech("~~oops~~ actually fine"),
            "oops actually fine"
        );
    }

    #[test]
    fn realistic_combined_reply_has_no_markdown_characters() {
        let input = "\
## Summary

I fixed **two** things in `parser.rs`:

- the [empty case](https://example.com/issue/1)
- the *trailing comma* case

```rust
fn main() {}
```

| file | lines |
| --- | --- |
| parser.rs | 12 |

Done.";
        let out = normalize_for_speech(input);
        for bad in ['*', '`', '#', '[', ']'] {
            assert!(!out.contains(bad), "{bad:?} survived in {out:?}");
        }
        assert!(out.contains("two"));
        assert!(out.contains("empty case"));
        assert!(out.contains("code block omitted."));
        assert!(out.contains("parser.rs, 12"));
        assert!(out.ends_with("Done."));
    }

    // ---- terminal noise ------------------------------------------------------

    #[test]
    fn ansi_colour_and_spinner_frames_never_reach_the_synthesizer() {
        let out = normalize_for_speech("\u{1b}[32m⠹\u{1b}[0m Building the crate now.");
        assert_eq!(out, "Building the crate now.");
    }

    // ---- html ----------------------------------------------------------------

    #[test]
    fn html_is_read_as_its_text() {
        let out = normalize_for_speech("<h1>Report</h1><p>Two tests <b>failed</b>.</p>");
        assert_eq!(out, "Report Two tests failed.");
    }

    #[test]
    fn html_entities_are_decoded() {
        let out = normalize_for_speech("<p>Tom &amp; Jerry, caf&eacute; style.</p>");
        assert_eq!(out, "Tom & Jerry, café style.");
    }

    #[test]
    fn html_script_bodies_are_not_read_aloud() {
        let out = normalize_for_speech("<p>ready</p><script>var x=1;</script>");
        assert_eq!(out, "ready");
    }

    #[test]
    fn generic_types_in_prose_are_not_mistaken_for_html() {
        let input = "Use Vec<String> when the length varies.";
        assert_eq!(normalize_for_speech(input), input);
    }

    // ---- json ----------------------------------------------------------------

    #[test]
    fn json_is_read_as_keys_and_values() {
        let out = normalize_for_speech("{\"status\": \"ok\", \"exit_code\": 0}");
        assert!(out.contains("status"), "got {out:?}");
        assert!(out.contains("ok"), "got {out:?}");
        assert!(out.contains("exit code"), "got {out:?}");
        assert!(!out.contains('{') && !out.contains('"'), "got {out:?}");
    }

    // ---- csv -----------------------------------------------------------------

    #[test]
    fn csv_is_read_as_rows_of_cells() {
        let out = normalize_for_speech("name,count\nalpha,3\nbeta,4");
        assert_eq!(out, "name, count. alpha, 3. beta, 4");
    }

    #[test]
    fn tsv_is_read_the_same_way_as_csv() {
        let out = normalize_for_speech("name\tcount\nalpha\t3");
        assert_eq!(out, "name, count. alpha, 3");
    }

    #[test]
    fn a_markdown_list_with_commas_is_not_read_as_a_table() {
        let out = normalize_for_speech("- alpha, three\n- beta, four");
        assert_eq!(out, "alpha, three beta, four");
    }

    // ---- length --------------------------------------------------------------

    #[test]
    fn long_text_is_cut_at_a_sentence_boundary() {
        let sentence = "This is a sentence of a reasonable length. ";
        let input = sentence.repeat(60);
        let out = normalize_for_speech(&input);
        assert!(out.chars().count() <= MAX_LEN + TRUNCATED_SUFFIX.chars().count());
        assert!(out.ends_with(TRUNCATED_SUFFIX), "got {out:?}");
        assert!(out.contains("length. This"));
    }

    #[test]
    fn short_text_is_not_truncated() {
        let out = normalize_for_speech("Short.");
        assert_eq!(out, "Short.");
    }
}

//! Turning "text-shaped" input into speakable prose.
//!
//! [`normalize_for_speech`](super::normalize::normalize_for_speech) used to assume one
//! input format — Markdown — because the only thing that fed it was a Claude reply. That
//! assumption was always too narrow: an assistant answers with HTML when you ask it about
//! HTML, tools print JSON, and terminal output arrives wrapped in ANSI colour and spinner
//! frames. A synthesizer has no way to know any of that is markup, so it reads it: angle
//! brackets become "less than", `&amp;` becomes "ampersand a m p semicolon", and a
//! progress bar becomes a minute of clicking.
//!
//! So this module holds the format-aware front end, one concern per function:
//!
//! * [`strip_ansi`] — escape sequences, which can appear in *any* of the formats below;
//! * [`strip_decoration`] — box drawing, block glyphs, braille spinner frames, zero-width
//!   characters: things that draw rather than say;
//! * [`looks_like_html`] / [`html_to_text`] — tags to structure, entities to characters;
//! * [`json_to_text`] — a whole-document JSON value flattened to its readable leaves.
//!
//! Nothing here is a parser in the strict sense, and it should not become one. The job is
//! to fail *soft*: text that only looks a bit like markup must come out no worse than it
//! went in, because being wrong here means mispronouncing a sentence, and the caller
//! cannot review what it never sees.

use std::fmt::Write as _;

/// Tag names that make a run of angle brackets HTML rather than prose.
///
/// Detection is name-based on purpose. `Vec<String>` and `a < b` are far more common in an
/// agent's reply than any HTML is, and both would fool a "contains `<` and `>`" test —
/// `String` is not a tag, so they survive intact.
const HTML_TAGS: &[&str] = &[
    "a",
    "abbr",
    "address",
    "article",
    "aside",
    "audio",
    "b",
    "blockquote",
    "body",
    "br",
    "button",
    "canvas",
    "caption",
    "cite",
    "code",
    "col",
    "colgroup",
    "dd",
    "del",
    "details",
    "div",
    "dl",
    "dt",
    "em",
    "embed",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "head",
    "header",
    "hr",
    "html",
    "i",
    "iframe",
    "img",
    "input",
    "ins",
    "kbd",
    "label",
    "legend",
    "li",
    "link",
    "main",
    "mark",
    "meta",
    "nav",
    "object",
    "ol",
    "option",
    "p",
    "param",
    "picture",
    "pre",
    "progress",
    "q",
    "s",
    "samp",
    "script",
    "section",
    "select",
    "small",
    "source",
    "span",
    "strong",
    "style",
    "sub",
    "summary",
    "sup",
    "svg",
    "table",
    "tbody",
    "td",
    "template",
    "textarea",
    "tfoot",
    "th",
    "thead",
    "time",
    "title",
    "tr",
    "track",
    "u",
    "ul",
    "var",
    "video",
    "wbr",
];

/// Tags whose boundary is a paragraph break — a full stop's worth of pause.
const BLOCK_TAGS: &[&str] = &[
    "address",
    "article",
    "aside",
    "blockquote",
    "details",
    "div",
    "dl",
    "fieldset",
    "figure",
    "figcaption",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hr",
    "main",
    "nav",
    "ol",
    "p",
    "pre",
    "section",
    "summary",
    "table",
    "ul",
];

/// Tags whose boundary is a line break — enough to keep two items apart.
const LINE_TAGS: &[&str] = &[
    "br", "caption", "dd", "dt", "li", "tbody", "tfoot", "thead", "tr",
];

/// Tags whose content is code for a machine, never words for a person.
const OPAQUE_TAGS: &[&str] = &["script", "style", "template", "svg"];

/// Named character entities worth decoding, chosen for what actually turns up in generated
/// HTML. Numeric forms (`&#39;`, `&#x2019;`) are handled generically and need no table.
const ENTITIES: &[(&str, char)] = &[
    ("amp", '&'),
    ("lt", '<'),
    ("gt", '>'),
    ("quot", '"'),
    ("apos", '\''),
    ("nbsp", ' '),
    ("ensp", ' '),
    ("emsp", ' '),
    ("thinsp", ' '),
    ("shy", '\u{ad}'),
    ("ndash", '\u{2013}'),
    ("mdash", '\u{2014}'),
    ("hellip", '\u{2026}'),
    ("ldquo", '"'),
    ("rdquo", '"'),
    ("lsquo", '\''),
    ("rsquo", '\''),
    ("laquo", '\u{ab}'),
    ("raquo", '\u{bb}'),
    ("copy", '\u{a9}'),
    ("reg", '\u{ae}'),
    ("trade", '\u{2122}'),
    ("deg", '\u{b0}'),
    ("plusmn", '\u{b1}'),
    ("times", '\u{d7}'),
    ("divide", '\u{f7}'),
    ("middot", '\u{b7}'),
    ("bull", '\u{2022}'),
    ("dagger", '\u{2020}'),
    ("sect", '\u{a7}'),
    ("para", '\u{b6}'),
    ("euro", '\u{20ac}'),
    ("pound", '\u{a3}'),
    ("yen", '\u{a5}'),
    ("cent", '\u{a2}'),
    ("larr", '\u{2190}'),
    ("rarr", '\u{2192}'),
    ("harr", '\u{2194}'),
    ("ne", '\u{2260}'),
    ("le", '\u{2264}'),
    ("ge", '\u{2265}'),
    ("frac12", '\u{bd}'),
    ("frac14", '\u{bc}'),
    ("sup2", '\u{b2}'),
    ("sup3", '\u{b3}'),
];

/// The Latin-1 entity names, in code point order starting at U+00C0. HTML assigned these
/// names to that block in order, so an entry's position in this list *is* its character —
/// which is both shorter and less error-prone than sixty hand-written pairs.
const LATIN1_ENTITIES: &[&str] = &[
    "Agrave", "Aacute", "Acirc", "Atilde", "Auml", "Aring", "AElig", "Ccedil", "Egrave", "Eacute",
    "Ecirc", "Euml", "Igrave", "Iacute", "Icirc", "Iuml", "ETH", "Ntilde", "Ograve", "Oacute",
    "Ocirc", "Otilde", "Ouml", "times", "Oslash", "Ugrave", "Uacute", "Ucirc", "Uuml", "Yacute",
    "THORN", "szlig", "agrave", "aacute", "acirc", "atilde", "auml", "aring", "aelig", "ccedil",
    "egrave", "eacute", "ecirc", "euml", "igrave", "iacute", "icirc", "iuml", "eth", "ntilde",
    "ograve", "oacute", "ocirc", "otilde", "ouml", "divide", "oslash", "ugrave", "uacute", "ucirc",
    "uuml", "yacute", "thorn", "yuml",
];

/// Remove ANSI/VT escape sequences.
///
/// Three shapes cover everything a terminal actually emits: CSI (`ESC [ … final`), which
/// carries colour and cursor movement; OSC (`ESC ] … BEL` or `ESC \`), which carries window
/// titles and hyperlinks; and the two-character sequences (`ESC ( B`, `ESC =`) that switch
/// character sets. A lone `ESC` with nothing recognizable after it is dropped rather than
/// spoken.
#[tracing::instrument(level = "debug", ret)]
pub fn strip_ansi(input: &str) -> String {
    let ch: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < ch.len() {
        if ch[i] != '\u{1b}' {
            out.push(ch[i]);
            i += 1;
            continue;
        }
        match ch.get(i + 1) {
            // CSI: parameter and intermediate bytes, then one final byte in @ ..= ~.
            Some('[') => {
                let mut j = i + 2;
                while j < ch.len() && !matches!(ch[j], '\u{40}'..='\u{7e}') {
                    j += 1;
                }
                i = (j + 1).min(ch.len());
            }
            // OSC / DCS / APC / PM: run to a string terminator (BEL, or ESC \).
            Some(']') | Some('P') | Some('^') | Some('_') => {
                let mut j = i + 2;
                while j < ch.len() {
                    if ch[j] == '\u{7}' {
                        j += 1;
                        break;
                    }
                    if ch[j] == '\u{1b}' && ch.get(j + 1) == Some(&'\\') {
                        j += 2;
                        break;
                    }
                    j += 1;
                }
                i = j.min(ch.len());
            }
            // Charset selection and friends: ESC plus one or two bytes.
            Some(&('(' | ')' | '*' | '+' | '#' | '%')) => i += 3,
            Some(_) => i += 2,
            None => i += 1,
        }
    }
    out
}

/// Is `c` a character that draws rather than says?
#[tracing::instrument(level = "debug", ret)]
fn is_decoration(c: char) -> bool {
    matches!(c,
        // C0 controls. Tab and newline survive: they are the structure the line-based
        // passes downstream read.
        '\u{0}'..='\u{8}' | '\u{b}'..='\u{1f}' | '\u{7f}'
        // Box drawing, block elements, geometric shapes: tables and progress bars.
        | '\u{2500}'..='\u{259f}' | '\u{25a0}'..='\u{25ff}'
        // Braille — not braille here, but the eight-dot spinner frames every CLI uses.
        | '\u{2800}'..='\u{28ff}'
        // Zero-width and directionality marks; a BOM landing mid-stream.
        | '\u{200b}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{feff}'
        // Variation selectors, which style the previous glyph and say nothing.
        | '\u{fe00}'..='\u{fe0f}'
    )
}

/// Drop the characters that exist to be looked at, not listened to. See [`is_decoration`].
///
/// Each one becomes a space rather than nothing: `a│b` is two things, and gluing them into
/// `ab` invents a word. Whitespace is collapsed at the end of the pipeline anyway.
#[tracing::instrument(level = "debug", ret)]
pub fn strip_decoration(input: &str) -> String {
    input
        .chars()
        .map(|c| if is_decoration(c) { ' ' } else { c })
        .collect()
}

/// Does this text contain real HTML?
///
/// True for a recognized tag name (see [`HTML_TAGS`]) inside a well-formed pair of angle
/// brackets, or an HTML comment. Deliberately conservative: a false positive here deletes
/// words from the middle of a sentence, while a false negative only leaves a stray bracket
/// that the synthesizer will not read aloud anyway.
#[tracing::instrument(level = "debug", ret)]
pub fn looks_like_html(input: &str) -> bool {
    if input.contains("<!--") {
        return true;
    }
    let ch: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i < ch.len() {
        if ch[i] == '<' {
            if let Some(tag) = parse_tag(&ch, i) {
                if HTML_TAGS.contains(&tag.name.as_str()) {
                    return true;
                }
                i = tag.end;
                continue;
            }
        }
        i += 1;
    }
    false
}

struct Tag {
    name: String,
    closing: bool,
    attrs: String,
    /// Index just past the closing `>`.
    end: usize,
}

/// Parse one `<…>` starting at `ch[at]`, or `None` if it is not a tag at all.
///
/// Attribute scanning tracks quotes so a `>` inside `title="a > b"` does not end the tag
/// early. An unterminated bracket returns `None` — the `<` is then literal text, which is
/// what a lone `<` in prose is.
#[tracing::instrument(level = "debug")]
fn parse_tag(ch: &[char], at: usize) -> Option<Tag> {
    let mut i = at + 1;
    let closing = ch.get(i) == Some(&'/');
    if closing {
        i += 1;
    }
    let name_start = i;
    while i < ch.len() && (ch[i].is_ascii_alphanumeric() || ch[i] == '-') {
        i += 1;
    }
    if i == name_start {
        return None;
    }
    let name: String = ch[name_start..i]
        .iter()
        .collect::<String>()
        .to_ascii_lowercase();
    let attr_start = i;
    let mut quote: Option<char> = None;
    while i < ch.len() {
        let c = ch[i];
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c == '>' => {
                return Some(Tag {
                    name,
                    closing,
                    attrs: ch[attr_start..i].iter().collect(),
                    end: i + 1,
                })
            }
            None if c == '<' => return None,
            None => {}
        }
        i += 1;
    }
    None
}

/// Read one attribute's value out of a tag's raw attribute text.
#[tracing::instrument(level = "debug", ret)]
fn attr_value(attrs: &str, want: &str) -> Option<String> {
    let lower = attrs.to_ascii_lowercase();
    let mut from = 0;
    while let Some(rel) = lower[from..].find(want) {
        let at = from + rel;
        let before_ok = at == 0
            || lower[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_whitespace());
        let rest = &attrs[at + want.len()..];
        let trimmed = rest.trim_start();
        if before_ok {
            if let Some(eq) = trimmed.strip_prefix('=') {
                let v = eq.trim_start();
                let value = match v.chars().next() {
                    Some(q @ ('"' | '\'')) => v[1..].split(q).next().unwrap_or("").to_string(),
                    _ => v.split_whitespace().next().unwrap_or("").to_string(),
                };
                return Some(value);
            }
        }
        from = at + want.len();
    }
    None
}

/// Flatten HTML to the words inside it.
///
/// Tags become whitespace whose *amount* carries the structure — a paragraph break for
/// block elements, a newline for list and table rows, `", "` between cells — so the
/// downstream line passes see the same shape they would have seen from Markdown. Entities
/// are decoded; `<script>`, `<style>` and `<svg>` bodies are dropped whole; an `<img>`
/// contributes its `alt` text, which is the one place HTML puts words that are not between
/// tags.
#[tracing::instrument(level = "debug", ret)]
pub fn html_to_text(input: &str) -> String {
    let ch: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut text = String::new();
    let mut i = 0;

    while i < ch.len() {
        if ch[i] == '<' {
            // A comment is content the author explicitly hid; hiding it from a listener
            // too is the consistent choice.
            if ch[i..].starts_with(&['<', '!', '-', '-']) {
                i = find_seq(&ch, i + 4, &['-', '-', '>'])
                    .map(|p| p + 3)
                    .unwrap_or(ch.len());
                continue;
            }
            if ch.get(i + 1) == Some(&'!') || ch.get(i + 1) == Some(&'?') {
                i = ch[i..]
                    .iter()
                    .position(|&c| c == '>')
                    .map_or(ch.len(), |p| i + p + 1);
                continue;
            }
            if let Some(tag) = parse_tag(&ch, i) {
                if HTML_TAGS.contains(&tag.name.as_str()) {
                    out.push_str(&decode_entities(&text));
                    text.clear();
                    if !tag.closing && OPAQUE_TAGS.contains(&tag.name.as_str()) {
                        i = skip_element(&ch, tag.end, &tag.name);
                        continue;
                    }
                    if !tag.closing && tag.name == "img" {
                        if let Some(alt) = attr_value(&tag.attrs, "alt") {
                            let alt = decode_entities(&alt);
                            if !alt.trim().is_empty() {
                                let _ = write!(out, " {} ", alt.trim());
                            }
                        }
                    }
                    out.push_str(separator_for(&tag.name, tag.closing));
                    i = tag.end;
                    continue;
                }
            }
        }
        text.push(ch[i]);
        i += 1;
    }
    out.push_str(&decode_entities(&text));
    tidy_cells(&out)
}

/// The whitespace a tag boundary stands for. Cell ends are the one case that needs a
/// visible mark: two table cells run together are one wrong word, not two right ones.
#[tracing::instrument(level = "debug", ret)]
fn separator_for(name: &str, closing: bool) -> &'static str {
    if (name == "td" || name == "th") && closing {
        return ", ";
    }
    if BLOCK_TAGS.contains(&name) {
        return "\n\n";
    }
    if LINE_TAGS.contains(&name) {
        return "\n";
    }
    // Inline tags (`<b>`, `<a>`, `<span>`) must not glue their neighbours together, but
    // must not split a word either: `<b>re</b>run` is one word in the rendered page.
    ""
}

/// Drop the `", "` a final `</td>` leaves dangling at the end of its row.
#[tracing::instrument(level = "debug", ret)]
fn tidy_cells(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for (n, line) in input.split('\n').enumerate() {
        if n > 0 {
            out.push('\n');
        }
        out.push_str(line.trim_end().trim_end_matches(',').trim_end());
    }
    out
}

#[tracing::instrument(level = "debug", ret)]
fn find_seq(ch: &[char], from: usize, needle: &[char]) -> Option<usize> {
    (from..ch.len().saturating_sub(needle.len() - 1)).find(|&i| ch[i..i + needle.len()] == *needle)
}

/// Skip to just past `</name>`, or to the end if it never closes.
#[tracing::instrument(level = "debug", ret)]
fn skip_element(ch: &[char], from: usize, name: &str) -> usize {
    let mut i = from;
    while i < ch.len() {
        if ch[i] == '<' && ch.get(i + 1) == Some(&'/') {
            if let Some(tag) = parse_tag(ch, i) {
                if tag.name == name {
                    return tag.end;
                }
            }
        }
        i += 1;
    }
    ch.len()
}

/// Decode `&amp;`-style character references (named, decimal and hexadecimal).
///
/// An unrecognized reference is left exactly as written. Guessing would be worse than
/// useless: `&foo;` read as text is a small oddity, whereas silently deleting it removes
/// content the listener has no way to recover.
#[tracing::instrument(level = "debug", ret)]
pub fn decode_entities(input: &str) -> String {
    if !input.contains('&') {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        // A reference is short; anything longer is an ampersand in prose.
        let end = after
            .char_indices()
            .take(12)
            .find(|&(_, c)| c == ';')
            .map(|(i, _)| i);
        match end.and_then(|e| resolve_entity(&after[..e]).map(|c| (c, e))) {
            Some((c, e)) => {
                out.push(c);
                rest = &after[e + 1..];
            }
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[tracing::instrument(level = "debug", ret)]
fn resolve_entity(body: &str) -> Option<char> {
    if let Some(num) = body.strip_prefix('#') {
        let code = match num.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => num.parse::<u32>().ok()?,
        };
        return char::from_u32(code);
    }
    if let Some(c) = ENTITIES
        .iter()
        .find(|(name, _)| *name == body)
        .map(|&(_, c)| c)
    {
        return Some(c);
    }
    LATIN1_ENTITIES
        .iter()
        .position(|name| *name == body)
        .and_then(|i| char::from_u32(0xc0 + i as u32))
}

/// Flatten a whole-document JSON value to its readable leaves, or `None` if the text is
/// not JSON.
///
/// A tool that answers in JSON is answering in a text format like any other, and the
/// alternative is hearing every brace, bracket and quotation mark. Keys are spoken because
/// they are usually the only label a value has; `_` and `-` become spaces so `exit_code`
/// is two words rather than one unpronounceable one.
#[tracing::instrument(level = "debug", ret)]
pub fn json_to_text(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if !(trimmed.starts_with('{') || trimmed.starts_with('[')) {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    let mut parts = Vec::new();
    flatten_json(&value, &mut parts);
    Some(parts.join(". "))
}

#[tracing::instrument(level = "debug", ret)]
fn flatten_json(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::Null => {}
        serde_json::Value::Bool(b) => out.push(b.to_string()),
        serde_json::Value::Number(n) => out.push(n.to_string()),
        serde_json::Value::String(s) => {
            if !s.trim().is_empty() {
                out.push(s.trim().to_string());
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                flatten_json(item, out);
            }
        }
        serde_json::Value::Object(map) => {
            for (key, val) in map {
                let label = key.replace(['_', '-'], " ");
                let mut inner = Vec::new();
                flatten_json(val, &mut inner);
                if inner.is_empty() {
                    continue;
                }
                out.push(format!("{label}: {}", inner.join(". ")));
            }
        }
    }
}

/// Delimiters worth testing for a table of values. `|` is absent: a pipe-delimited table is
/// Markdown's, and [`super::normalize`] already flattens those row by row.
const CSV_DELIMS: &[char] = &[',', '\t', ';'];

/// Longest a field can be before the text is prose that happens to contain commas rather
/// than a table of values.
const CSV_MAX_FIELD: usize = 100;

/// Most words a field can hold before it is a clause, not a value. Long free-text columns
/// exist, but a document made mostly of them is a paragraph.
const CSV_MAX_WORDS: usize = 12;

/// Flatten delimited rows (CSV, TSV, semicolon-separated) to spoken text, or `None` if the
/// text is not a table.
///
/// Rows become `", "`-joined cells joined by `". "` — the same shape [`super::normalize`]
/// already gives a Markdown table, so a table sounds the same whichever syntax it arrived
/// in. The header row is simply the first row: it is read once, as labels, and then the
/// values follow.
///
/// Detection is the whole difficulty, because English is full of commas. A paragraph is
/// rejected on any of four counts: fewer than two lines, rows that disagree about how many
/// fields they have, a single field, or a field long enough to be a sentence. Prose fails
/// the second test almost always and the fourth immediately.
#[tracing::instrument(level = "debug", ret)]
pub fn csv_to_text(input: &str) -> Option<String> {
    let lines: Vec<&str> = input.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.len() < 2 {
        return None;
    }
    let delim = CSV_DELIMS
        .iter()
        .copied()
        .find(|&d| csv_fields(lines[0], d).len() >= 2)?;

    let mut rows = Vec::with_capacity(lines.len());
    let width = csv_fields(lines[0], delim).len();
    for line in &lines {
        let fields = csv_fields(line, delim);
        if fields.len() != width {
            return None;
        }
        if fields.iter().any(|f| f.chars().count() > CSV_MAX_FIELD) {
            return None;
        }
        rows.push(fields);
    }
    // A table is values, and a value is not a sentence. One row of prose that happens to
    // have the right number of commas is possible; a whole document of them is not. Three
    // marks give a row away: a sentence break inside a field, a run of words too long to
    // be a value, and a final field that ends the way a sentence ends.
    let prose_rows = rows.iter().filter(|r| is_prose_row(r)).count();
    if prose_rows * 2 > rows.len() {
        return None;
    }
    Some(
        rows.iter()
            .map(|r| {
                r.iter()
                    .map(|f| f.trim())
                    .filter(|f| !f.is_empty())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .filter(|r| !r.is_empty())
            .collect::<Vec<_>>()
            .join(". "),
    )
}

/// Does this row read like a sentence someone wrote rather than a row of values?
#[tracing::instrument(level = "debug", ret)]
fn is_prose_row(fields: &[String]) -> bool {
    if fields.iter().any(|f| f.contains(". ")) {
        return true;
    }
    if fields
        .iter()
        .any(|f| f.split_whitespace().count() > CSV_MAX_WORDS)
    {
        return true;
    }
    fields
        .last()
        .map(|f| f.trim_end().ends_with(['.', '!', '?']))
        .unwrap_or(false)
}

/// Split one delimited line into fields, honouring RFC 4180 quoting: a delimiter inside
/// `"…"` is data, and `""` inside a quoted field is one literal quote.
#[tracing::instrument(level = "debug", ret)]
fn csv_fields(line: &str, delim: char) -> Vec<String> {
    let mut fields = Vec::new();
    let mut cur = String::new();
    let mut chars = line.chars().peekable();
    let mut quoted = false;
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            c if c == delim && !quoted => fields.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    fields.push(cur);
    fields
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- ANSI ----------------------------------------------------------------

    #[test]
    fn strips_colour_and_cursor_sequences() {
        let input = "\u{1b}[31mred\u{1b}[0m and \u{1b}[2K\u{1b}[1;5Hplain";
        assert_eq!(strip_ansi(input), "red and plain");
    }

    #[test]
    fn strips_osc_title_sequence_with_either_terminator() {
        assert_eq!(strip_ansi("\u{1b}]0;a title\u{7}done"), "done");
        assert_eq!(strip_ansi("\u{1b}]0;a title\u{1b}\\done"), "done");
    }

    #[test]
    fn leaves_plain_text_untouched() {
        let input = "Nothing to strip here — just prose, with punctuation!";
        assert_eq!(strip_ansi(input), input);
        assert_eq!(strip_decoration(input), input);
    }

    // ---- decoration ----------------------------------------------------------

    #[test]
    fn strips_spinner_frames_and_box_drawing() {
        assert_eq!(strip_decoration("⠋⠙⠹ working").trim(), "working");
        assert_eq!(strip_decoration("┌──┐").trim(), "");
    }

    #[test]
    fn dropped_characters_leave_a_space_so_words_do_not_fuse() {
        // The box edge between two cells is the only thing separating them.
        assert_eq!(strip_decoration("one│two"), "one two");
    }

    #[test]
    fn keeps_tabs_and_newlines() {
        assert_eq!(strip_decoration("a\tb\nc"), "a\tb\nc");
    }

    // ---- HTML detection ------------------------------------------------------

    #[test]
    fn detects_real_html() {
        assert!(looks_like_html("<p>Hello <b>there</b></p>"));
        assert!(looks_like_html(
            "<!DOCTYPE html><html><body>hi</body></html>"
        ));
    }

    #[test]
    fn generics_and_comparisons_are_not_html() {
        assert!(!looks_like_html("Use Vec<String> for the buffer."));
        assert!(!looks_like_html("The guard checks a < b and b > c."));
        assert!(!looks_like_html("Prose with no angle brackets at all."));
    }

    // ---- HTML to text --------------------------------------------------------

    #[test]
    fn html_keeps_text_and_breaks_blocks() {
        let out = html_to_text("<h1>Title</h1><p>First.</p><p>Second.</p>");
        assert!(out.contains("Title"));
        assert!(out.contains("First."));
        assert!(out.contains("Second."));
        assert!(!out.contains('<'));
    }

    #[test]
    fn inline_tags_do_not_split_a_word() {
        assert_eq!(html_to_text("<p>re<b>run</b></p>").trim(), "rerun");
    }

    #[test]
    fn script_and_style_bodies_are_dropped_whole() {
        let out = html_to_text("<p>keep</p><script>var x = 1 < 2;</script><style>p{}</style>");
        assert!(out.contains("keep"));
        assert!(!out.contains("var x"));
        assert!(!out.contains("p{}"));
    }

    #[test]
    fn comments_are_dropped() {
        let out = html_to_text("<p>visible</p><!-- hidden note -->");
        assert!(out.contains("visible"));
        assert!(!out.contains("hidden"));
    }

    #[test]
    fn image_alt_text_is_spoken() {
        let out = html_to_text(r#"<img src="x.png" alt="a red bicycle">"#);
        assert!(out.contains("a red bicycle"));
    }

    #[test]
    fn table_cells_are_separated_by_commas_and_rows_by_lines() {
        let out =
            html_to_text("<table><tr><td>one</td><td>two</td></tr><tr><td>three</td></tr></table>");
        let flat = out.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(flat.contains("one, two"), "got {flat:?}");
        assert!(flat.contains("three"));
    }

    #[test]
    fn attribute_may_contain_a_closing_bracket() {
        let out = html_to_text(r#"<p title="a > b">text</p>"#);
        assert_eq!(out.trim(), "text");
    }

    #[test]
    fn an_unterminated_bracket_is_literal_prose() {
        let out = html_to_text("<p>a < b always</p>");
        assert!(out.contains("a < b always"), "got {out:?}");
    }

    // ---- entities ------------------------------------------------------------

    #[test]
    fn decodes_named_and_numeric_entities() {
        assert_eq!(decode_entities("a &amp; b &lt; c"), "a & b < c");
        assert_eq!(decode_entities("&#65;&#x42;"), "AB");
        assert_eq!(decode_entities("caf&eacute;"), "café");
    }

    #[test]
    fn an_unknown_entity_is_left_alone_rather_than_guessed_at() {
        assert_eq!(decode_entities("&notareal; thing"), "&notareal; thing");
        assert_eq!(decode_entities("Q&A session"), "Q&A session");
    }

    // ---- JSON ----------------------------------------------------------------

    #[test]
    fn flattens_an_object_to_key_and_value() {
        let out = json_to_text(r#"{"status":"ok","exit_code":0}"#).expect("is json");
        assert!(out.contains("status"), "got {out:?}");
        assert!(out.contains("ok"), "got {out:?}");
        // Snake case is read as words, not as one run-on token.
        assert!(out.contains("exit code"), "got {out:?}");
    }

    #[test]
    fn flattens_an_array() {
        let out = json_to_text(r#"["alpha","beta"]"#).expect("is json");
        assert!(out.contains("alpha") && out.contains("beta"));
    }

    #[test]
    fn prose_is_not_json() {
        assert!(json_to_text("Just a sentence.").is_none());
        assert!(json_to_text("{ not actually json").is_none());
    }

    // ---- CSV -----------------------------------------------------------------

    #[test]
    fn flattens_comma_separated_rows() {
        let out = csv_to_text("name,count\nalpha,3\nbeta,4").expect("is csv");
        assert!(out.contains("name, count"), "got {out:?}");
        assert!(out.contains("alpha, 3"), "got {out:?}");
    }

    #[test]
    fn flattens_tab_separated_rows() {
        let out = csv_to_text("name\tcount\nalpha\t3").expect("is csv");
        assert!(out.contains("name, count"), "got {out:?}");
    }

    #[test]
    fn honours_rfc4180_quoting() {
        let out = csv_to_text("a,b\n\"one, two\",\"say \"\"hi\"\"\"").expect("is csv");
        assert!(out.contains("one, two"), "got {out:?}");
        assert!(out.contains("say \"hi\""), "got {out:?}");
    }

    #[test]
    fn prose_with_commas_is_not_csv() {
        let prose = "I ran the tests, and they passed on the first try after the fix landed.\n\
                     Then I looked at the second failure, which turned out to be unrelated to it.";
        assert!(csv_to_text(prose).is_none());
    }

    #[test]
    fn ragged_rows_are_not_csv() {
        assert!(csv_to_text("a,b,c\nd,e").is_none());
    }

    #[test]
    fn a_single_column_is_not_csv() {
        assert!(csv_to_text("alpha\nbeta\ngamma").is_none());
    }
}

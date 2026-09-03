//! `hyperpanes ctl …` — the workspace's own command line over the running control API.
//!
//! This is the tool surface the always-on **Hyperpane** tab hands its agent (see
//! `resources/claude/hyperpane/`). The MCP server (`hyperpanes-mcp`) is a separate npm package
//! and can't grow with this repo; a subcommand of the binary itself always matches the app it
//! is talking to, needs no install step, and is reachable from any shell — including one inside
//! a pane.
//!
//! Every verb is a thin, honest wrapper over an HTTP route: the named ones exist because an
//! agent should not have to remember JSON shapes, and the raw `get`/`post`/`patch`/`command`
//! passthroughs exist so a route this file never learned about is still reachable. Machine
//! verbs print JSON; the three browsing verbs (`tabs`, `panes`, `read`) print text, because
//! their whole job is to be read.
//!
//! Exit codes: 0 ok · 1 the server said no (or isn't running) · 2 usage.

use std::collections::BTreeMap;

use serde_json::{json, Value};

use crate::control_cli::{self, Conn};

#[tracing::instrument(level = "debug", ret)]
pub fn wants_ctl(argv: &[String]) -> bool {
    argv.get(1).map(|a| a == "ctl").unwrap_or(false)
}

const USAGE: &str = "\
usage: hyperpanes ctl <verb> [args]

  Discovery
    health                          is the control API up, and what does it allow
    state                           the whole windows→tabs→panes tree, as JSON
    tabs                            that tree as an outline, one line per tab and pane
    panes                           one line per pane: id, tab, status, label
    settings                        the app's preferences, as JSON

  Terminals
    read <pane> [--tail N] [--raw] [--screen] [--wait]
    send <pane> <text…>             type text, no Enter
    submit <pane> <text…>           type text, then Enter
    keys <pane> <key>…              named keys, e.g. enter escape ctrl+c up

  Panes
    new-pane [--cwd D] [--cmd C] [--label L] [--color #rrggbb] [--shell S]
             [--project P] [--window N]        (lands in that window's active tab)
    close-pane <pane>
    restart-pane <pane>
    focus-pane <pane>
    rename-pane <pane> <title>
    recolor-pane <pane> <#rrggbb>
    layout <tab> <name>             auto | single | columns | rows | grid |
                                    main-stack | grid-<cols>x<rows>

  Tabs
    new-tab [--window N] [--title T] [--cwd D]
    close-tab <tab>
    rename-tab <tab> <title>
    focus-tab <tab>
    move-tab <tab> <index>

  Preferences
    set <key> <value>               one setting; value is JSON if it parses, else a string
    set-json <json>                 a whole patch object

  Raw (anything the verbs above don't cover)
    get <path>
    post <path> [json]
    patch <path> [json]
    command <json>                  POST /command with a verb object

Ids: a pane id comes from `panes`; a tab id is \"{window}:{index}\" and comes from `tabs`.
Tab ids are POSITIONAL — re-read `tabs` after anything that reorders or closes one.";

#[tracing::instrument(level = "debug", ret)]
pub fn run(argv: &[String]) -> std::io::Result<()> {
    let verb = argv.get(2).map(String::as_str).unwrap_or("");
    if verb.is_empty() || verb == "help" || verb == "--help" || verb == "-h" {
        println!("{USAGE}");
        return Ok(());
    }
    let args: Vec<String> = argv[3..].to_vec();
    let conn = control_cli::connect().unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });
    match verb {
        // ---- discovery ----
        "health" => print_json(get(&conn, "/health")?),
        "state" => print_json(get(&conn, "/state")?),
        "settings" => print_json(get(&conn, "/settings")?),
        "tabs" => print_outline(&get(&conn, "/state")?),
        "panes" => print_panes(&get(&conn, "/state")?),

        // ---- terminals ----
        "read" => {
            let (pane, flags) =
                split_flags(&args, "read <pane> [--tail N] [--raw] [--screen] [--wait]");
            let mut q: Vec<String> = Vec::new();
            // Stripping ANSI is the default here and nowhere else: an agent reading a pane wants
            // the words, not the cursor choreography. `--raw` opts back into the bytes.
            if !flags.contains_key("raw") {
                q.push("strip=1".into());
            }
            if flags.contains_key("screen") {
                q.push("mode=screen".into());
            }
            if flags.contains_key("wait") {
                q.push("waitForIdle=1".into());
            }
            if let Some(n) = flags.get("tail") {
                q.push(format!("tail={n}"));
            }
            let body = get(&conn, &format!("/panes/{pane}/output?{}", q.join("&")))?;
            // The text is the point; print it bare and put the metadata on stderr so a pipe
            // gets exactly the terminal's content.
            eprintln!(
                "[{} · {}]",
                pane,
                body.get("status").and_then(Value::as_str).unwrap_or("?")
            );
            print!(
                "{}",
                body.get("output").and_then(Value::as_str).unwrap_or("")
            );
        }
        "send" | "submit" => {
            let pane = need(args.first(), "send <pane> <text…>");
            let text = args[1..].join(" ");
            if text.is_empty() {
                usage("send <pane> <text…>");
            }
            print_json(post(
                &conn,
                &format!("/panes/{pane}/input"),
                json!({ "data": text, "submit": verb == "submit" }),
            )?);
        }
        "keys" => {
            let pane = need(args.first(), "keys <pane> <key>…");
            let keys: Vec<&str> = args[1..].iter().map(String::as_str).collect();
            if keys.is_empty() {
                usage("keys <pane> <key>…");
            }
            print_json(post(
                &conn,
                &format!("/panes/{pane}/input"),
                json!({ "keys": keys }),
            )?);
        }

        // ---- panes ----
        "new-pane" => {
            let (_, flags) = split_flags_optional(&args);
            // The spawn spec goes UNDER `pane` — a flat one is rejected by the server, which is
            // the right call: a typo'd top-level `command` would otherwise silently spawn a
            // default shell. The pane lands in the window's active tab.
            let mut spec = json!({});
            put_str(&mut spec, "cwd", flags.get("cwd"));
            put_str(&mut spec, "command", flags.get("cmd"));
            put_str(&mut spec, "label", flags.get("label"));
            put_str(&mut spec, "color", flags.get("color"));
            put_str(&mut spec, "shell", flags.get("shell"));
            put_str(&mut spec, "project", flags.get("project"));
            let mut cmd = json!({ "type": "newPane", "pane": spec });
            if let Some(w) = flags.get("window").and_then(|w| w.parse::<i64>().ok()) {
                cmd["windowId"] = json!(w);
            }
            print_json(post(&conn, "/command", cmd)?);
        }
        "close-pane" => print_json(pane_verb(&conn, "closePane", &args, "close-pane <pane>")?),
        "restart-pane" => print_json(pane_verb(
            &conn,
            "restartPane",
            &args,
            "restart-pane <pane>",
        )?),
        "focus-pane" => print_json(pane_verb(&conn, "focusPane", &args, "focus-pane <pane>")?),
        "rename-pane" => {
            let pane = need(args.first(), "rename-pane <pane> <title>");
            let title = args[1..].join(" ");
            if title.is_empty() {
                usage("rename-pane <pane> <title>");
            }
            print_json(post(
                &conn,
                "/command",
                json!({ "type": "renamePane", "paneId": pane, "label": title }),
            )?);
        }
        "recolor-pane" => {
            let pane = need(args.first(), "recolor-pane <pane> <#rrggbb>");
            let color = need(args.get(1), "recolor-pane <pane> <#rrggbb>");
            print_json(post(
                &conn,
                "/command",
                json!({ "type": "recolorPane", "paneId": pane, "color": color }),
            )?);
        }
        "layout" => {
            let tab = need(args.first(), "layout <tab> <name>");
            let name = need(args.get(1), "layout <tab> <name>");
            print_json(post(
                &conn,
                "/command",
                json!({ "type": "setLayout", "tabId": tab, "layout": name }),
            )?);
        }

        // ---- tabs ----
        "new-tab" => {
            let (_, flags) = split_flags_optional(&args);
            let mut cmd = json!({ "type": "newTab" });
            if let Some(w) = flags.get("window").and_then(|w| w.parse::<i64>().ok()) {
                cmd["windowId"] = json!(w);
            }
            put_str(&mut cmd, "title", flags.get("title"));
            put_str(&mut cmd, "cwd", flags.get("cwd"));
            print_json(post(&conn, "/command", cmd)?);
        }
        "close-tab" => print_json(tab_verb(&conn, "closeTab", &args, "close-tab <tab>")?),
        "focus-tab" => print_json(tab_verb(&conn, "focusTab", &args, "focus-tab <tab>")?),
        "rename-tab" => {
            let tab = need(args.first(), "rename-tab <tab> <title>");
            let title = args[1..].join(" ");
            if title.is_empty() {
                usage("rename-tab <tab> <title>");
            }
            print_json(post(
                &conn,
                "/command",
                json!({ "type": "renameTab", "tabId": tab, "title": title }),
            )?);
        }
        "move-tab" => {
            let tab = need(args.first(), "move-tab <tab> <index>");
            let to: usize = need(args.get(1), "move-tab <tab> <index>")
                .parse()
                .unwrap_or_else(|_| usage("move-tab <tab> <index>   (index is a number)"));
            print_json(post(
                &conn,
                "/command",
                json!({ "type": "moveTab", "tabId": tab, "to": to }),
            )?);
        }

        // ---- preferences ----
        "set" => {
            let key = need(args.first(), "set <key> <value>");
            let raw = args[1..].join(" ");
            if raw.is_empty() {
                usage("set <key> <value>");
            }
            // `set fontPx 15` should send a number and `set defaultShell zsh` a string, without
            // the caller having to know which is which — so try JSON first and fall back to the
            // literal text. (`set editorCommand "code -w"` therefore stays a string.)
            let value = serde_json::from_str::<Value>(&raw).unwrap_or_else(|_| json!(raw));
            print_json(patch(&conn, "/settings", json!({ key: value }))?);
        }
        "set-json" => {
            let body = parse_json(args.first(), "set-json <json>");
            print_json(patch(&conn, "/settings", body)?);
        }

        // ---- raw ----
        "get" => print_json(get(&conn, &path_arg(args.first(), "get <path>"))?),
        "post" => {
            let p = path_arg(args.first(), "post <path> [json]");
            let body = args.get(1).map_or(json!({}), |s| {
                serde_json::from_str(s).unwrap_or_else(|e| usage(&format!("bad json: {e}")))
            });
            print_json(post(&conn, &p, body)?);
        }
        "patch" => {
            let p = path_arg(args.first(), "patch <path> [json]");
            let body = args.get(1).map_or(json!({}), |s| {
                serde_json::from_str(s).unwrap_or_else(|e| usage(&format!("bad json: {e}")))
            });
            print_json(patch(&conn, &p, body)?);
        }
        "command" => {
            let body = parse_json(args.first(), "command <json>");
            print_json(post(&conn, "/command", body)?);
        }

        other => {
            eprintln!("unknown verb '{other}'\n\n{USAGE}");
            std::process::exit(2);
        }
    }
    Ok(())
}

// ---- HTTP ----------------------------------------------------------------------------------

#[tracing::instrument(level = "debug", ret, skip(conn))]
fn get(conn: &Conn, path: &str) -> std::io::Result<Value> {
    send(conn.client.get(format!("{}{path}", conn.base)), conn, path)
}

#[tracing::instrument(level = "debug", ret, skip(conn))]
fn post(conn: &Conn, path: &str, body: Value) -> std::io::Result<Value> {
    send(
        conn.client.post(format!("{}{path}", conn.base)).json(&body),
        conn,
        path,
    )
}

#[tracing::instrument(level = "debug", ret, skip(conn))]
fn patch(conn: &Conn, path: &str, body: Value) -> std::io::Result<Value> {
    send(
        conn.client
            .patch(format!("{}{path}", conn.base))
            .json(&body),
        conn,
        path,
    )
}

/// Send, and turn anything that isn't a 2xx into a message on stderr plus exit 1 — an agent
/// reading stdout should never have to tell a successful response from an error object.
#[tracing::instrument(level = "debug", ret, skip(conn))]
fn send(req: reqwest::blocking::RequestBuilder, conn: &Conn, path: &str) -> std::io::Result<Value> {
    let resp = req
        .bearer_auth(&conn.token)
        .send()
        .map_err(|e| std::io::Error::other(format!("{path}: {e}")))?;
    let status = resp.status();
    let text = resp.text().unwrap_or_default();
    if !status.is_success() {
        let detail = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string))
            .unwrap_or(text);
        eprintln!("{path}: {status} — {detail}");
        std::process::exit(1);
    }
    Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
}

// ---- output --------------------------------------------------------------------------------

#[tracing::instrument(level = "debug", ret)]
fn print_json(v: Value) {
    println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
}

/// `/state` as an outline. The point is that one screenful answers "what is open, and what is
/// each thing's id" — the two questions every other verb needs answered first.
#[tracing::instrument(level = "debug", ret)]
fn print_outline(state: &Value) {
    for w in state
        .get("windows")
        .and_then(Value::as_array)
        .map_or(&[][..], |a| a)
    {
        let wid = w.get("windowId").and_then(Value::as_i64).unwrap_or(-1);
        let active = w.get("activeTabId").and_then(Value::as_str).unwrap_or("");
        println!("window {wid}");
        for t in w
            .get("tabs")
            .and_then(Value::as_array)
            .map_or(&[][..], |a| a)
        {
            let id = t.get("id").and_then(Value::as_str).unwrap_or("?");
            let mark = if id == active { "*" } else { " " };
            println!(
                "{mark} {id}  {}  [{}]",
                t.get("title").and_then(Value::as_str).unwrap_or(""),
                t.get("layout").and_then(Value::as_str).unwrap_or("")
            );
            for p in t
                .get("panes")
                .and_then(Value::as_array)
                .map_or(&[][..], |a| a)
            {
                println!(
                    "      {}  {}  ({})",
                    p.get("id").and_then(Value::as_str).unwrap_or("?"),
                    p.get("label").and_then(Value::as_str).unwrap_or(""),
                    p.get("status").and_then(Value::as_str).unwrap_or("?")
                );
            }
        }
    }
}

/// Every pane, flat, with the tab it sits in — the listing to grep when you know a pane by its
/// title and need its id.
#[tracing::instrument(level = "debug", ret)]
fn print_panes(state: &Value) {
    for w in state
        .get("windows")
        .and_then(Value::as_array)
        .map_or(&[][..], |a| a)
    {
        for t in w
            .get("tabs")
            .and_then(Value::as_array)
            .map_or(&[][..], |a| a)
        {
            let tab = t.get("id").and_then(Value::as_str).unwrap_or("?");
            for p in t
                .get("panes")
                .and_then(Value::as_array)
                .map_or(&[][..], |a| a)
            {
                println!(
                    "{}\t{}\t{}\t{}",
                    p.get("id").and_then(Value::as_str).unwrap_or("?"),
                    tab,
                    p.get("status").and_then(Value::as_str).unwrap_or("?"),
                    p.get("label").and_then(Value::as_str).unwrap_or("")
                );
            }
        }
    }
}

// ---- argument plumbing ---------------------------------------------------------------------

fn usage(msg: &str) -> ! {
    eprintln!("usage: hyperpanes ctl {msg}");
    std::process::exit(2);
}

#[tracing::instrument(level = "debug", ret)]
fn need<'a>(v: Option<&'a String>, msg: &str) -> &'a str {
    match v.map(String::as_str).filter(|s| !s.is_empty()) {
        Some(s) => s,
        None => usage(msg),
    }
}

#[tracing::instrument(level = "debug", ret)]
fn path_arg(v: Option<&String>, msg: &str) -> String {
    let p = need(v, msg);
    if p.starts_with('/') {
        p.to_string()
    } else {
        format!("/{p}")
    }
}

#[tracing::instrument(level = "debug", ret)]
fn parse_json(v: Option<&String>, msg: &str) -> Value {
    let raw = need(v, msg);
    serde_json::from_str(raw).unwrap_or_else(|e| usage(&format!("{msg}   (bad json: {e})")))
}

/// Split `<positional> [--flag value | --flag]` into the first positional and a flag map.
#[tracing::instrument(level = "debug", ret)]
fn split_flags(args: &[String], msg: &str) -> (String, BTreeMap<String, String>) {
    let (pos, flags) = split_flags_optional(args);
    match pos.into_iter().next() {
        Some(p) => (p, flags),
        None => usage(msg),
    }
}

/// The same split with no required positional. A `--flag` with no value is recorded as present
/// with an empty value, so `flags.contains_key("raw")` is the test for a bare switch.
#[tracing::instrument(level = "debug", ret)]
fn split_flags_optional(args: &[String]) -> (Vec<String>, BTreeMap<String, String>) {
    let mut pos = Vec::new();
    let mut flags = BTreeMap::new();
    let mut i = 0;
    while i < args.len() {
        if let Some(name) = args[i].strip_prefix("--") {
            let takes_value = args
                .get(i + 1)
                .is_some_and(|v| !v.starts_with("--") || name == "cmd");
            if takes_value {
                flags.insert(name.to_string(), args[i + 1].clone());
                i += 2;
            } else {
                flags.insert(name.to_string(), String::new());
                i += 1;
            }
        } else {
            pos.push(args[i].clone());
            i += 1;
        }
    }
    (pos, flags)
}

#[tracing::instrument(level = "debug", ret)]
fn put_str(cmd: &mut Value, key: &str, val: Option<&String>) {
    if let Some(v) = val.filter(|v| !v.is_empty()) {
        cmd[key] = json!(v);
    }
}

#[tracing::instrument(level = "debug", ret, skip(conn))]
fn pane_verb(conn: &Conn, ty: &str, args: &[String], msg: &str) -> std::io::Result<Value> {
    let pane = need(args.first(), msg);
    post(conn, "/command", json!({ "type": ty, "paneId": pane }))
}

#[tracing::instrument(level = "debug", ret, skip(conn))]
fn tab_verb(conn: &Conn, ty: &str, args: &[String], msg: &str) -> std::io::Result<Value> {
    let tab = need(args.first(), msg);
    post(conn, "/command", json!({ "type": ty, "tabId": tab }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn wants_ctl_only_matches_the_subcommand() {
        assert!(wants_ctl(&s(&["hyperpanes", "ctl", "panes"])));
        assert!(!wants_ctl(&s(&["hyperpanes"])));
        assert!(!wants_ctl(&s(&["hyperpanes", "-c", "ctl"])));
    }

    #[test]
    fn flags_split_from_positionals_and_bare_switches_are_present_but_empty() {
        let (pos, flags) = split_flags_optional(&s(&["p1", "--tail", "40", "--raw"]));
        assert_eq!(pos, vec!["p1".to_string()]);
        assert_eq!(flags.get("tail").map(String::as_str), Some("40"));
        assert_eq!(flags.get("raw").map(String::as_str), Some(""));
    }

    #[test]
    fn a_switch_followed_by_another_switch_does_not_swallow_it() {
        // `--wait --tail 5`: `wait` must not eat `--tail` as its value.
        let (_, flags) = split_flags_optional(&s(&["p1", "--wait", "--tail", "5"]));
        assert_eq!(flags.get("wait").map(String::as_str), Some(""));
        assert_eq!(flags.get("tail").map(String::as_str), Some("5"));
    }

    #[test]
    fn cmd_takes_its_value_even_when_the_value_looks_like_a_flag() {
        // `--cmd "--version"` is a real thing to want to run.
        let (_, flags) = split_flags_optional(&s(&["--cmd", "--version"]));
        assert_eq!(flags.get("cmd").map(String::as_str), Some("--version"));
    }

    #[test]
    fn a_raw_path_gets_its_leading_slash() {
        assert_eq!(path_arg(Some(&"queues".to_string()), "x"), "/queues");
        assert_eq!(path_arg(Some(&"/queues".to_string()), "x"), "/queues");
    }
}

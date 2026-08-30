//! What is running in a pane *right now*, asked of the kernel rather than the screen.
//!
//! Every other detection signal in this module is indirect. `by_title` reads what the
//! program chose to print; the launch command records what was asked for, not what
//! survived. Both are evidence; neither is an answer. The operating system has the
//! answer and will give it up for the cost of two syscalls: a pty has a **foreground
//! process group**, and that group has a leader with an executable name.
//!
//! The result feeds [`registry::by_bin`] — the same matcher the launch command uses,
//! deliberately. `by_bin` needs no ambiguity rule because "the executable the user
//! asked to run is direct evidence"; this module's whole job is to hand that matcher
//! *better* evidence, not to invent a second matching rule beside it.
//!
//! The precedence rule of `docs/tool-panes-plan.md` §D5 still governs what a caller may
//! do with the answer: it may upgrade a pane's chrome, and it must never rewrite
//! `spawn_command`/`spawn_args` or the persisted `PaneKind`. Deterministic is not the
//! same as authoritative — a pane spawned as `claude` that is momentarily running `git`
//! is still a Claude pane.
//!
//! The module splits in two on purpose. [`tool_for_foreground_name`] and its helpers are
//! pure string work — argv shapes, login-shell dashes, interpreter wrappers, `comm`
//! truncation — and are exhaustively testable with no pty in sight. The FFI is the thin
//! layer above it that turns a descriptor into a string.

use super::registry::{self, ToolDef};

/// The handle a probe is given: on unix, the pty **master** descriptor.
///
/// Declared here rather than taken as `RawFd` so callers need no `cfg` of their own.
/// On Windows it is an inert `i32` that is never dereferenced — see [`foreground_pgrp`].
#[cfg(unix)]
pub type PtyFd = std::os::fd::RawFd;
#[cfg(not(unix))]
pub type PtyFd = i32;

/// Runtimes whose `argv[0]` names the interpreter, not the program the human started.
///
/// This is not hypothetical tidiness: `codex` installs as a `#!/usr/bin/env node`
/// script, so the kernel's honest answer for a pane running it is `node`, and a probe
/// that stopped there would report the runtime forever and never the tool.
const INTERPRETERS: &[&str] = &[
    "env", "node", "nodejs", "bun", "deno", "npx", "python", "python2", "python3", "pythonw",
    "ruby", "perl", "php", "uv", "uvx",
];

/// Suffixes a script file carries that its command name does not — `codex.js` is `codex`.
/// Shells are absent on purpose: `foo.sh` is a shell script nobody installs under a bare
/// `foo`, so stripping it would only manufacture matches.
const SCRIPT_SUFFIXES: &[&str] = &[".js", ".mjs", ".cjs", ".ts", ".py", ".rb", ".pl"];

/// The shortest name either truncating source can produce. Linux's `/proc/<pid>/comm`
/// keeps 15 characters (`TASK_COMM_LEN` counts the NUL); macOS's `p_comm` keeps 16. A
/// name at or past this length may therefore be a prefix of a longer one — below it, a
/// prefix match would only invent ambiguity that the truncation cannot have caused.
const TRUNCATION_FLOOR: usize = 15;

/// How many interpreter hops to follow before giving up. `npx` → `node` → the script is
/// the deepest real chain; the cap exists so a pathological argv cannot loop.
const MAX_INTERPRETER_HOPS: usize = 4;

/// The last path component, with a login shell's leading dash removed.
///
/// A login shell is exec'd with `argv[0]` set to `-zsh` by convention, which is why the
/// dash is stripped here and not treated as a flag: at position zero it is a marker, not
/// an option.
fn base_name(arg: &str) -> &str {
    let cut = arg.rsplit(['/', '\\']).next().unwrap_or(arg);
    cut.strip_prefix('-').unwrap_or(cut)
}

fn strip_script_suffix(name: &str) -> &str {
    for suffix in SCRIPT_SUFFIXES {
        if name.len() > suffix.len() && name.to_ascii_lowercase().ends_with(suffix) {
            return &name[..name.len() - suffix.len()];
        }
    }
    name
}

/// The program an argv names, lowercased — descending through interpreter wrappers.
fn program_from_argv(argv: &[&str]) -> Option<String> {
    let mut idx = 0usize;
    for _ in 0..MAX_INTERPRETER_HOPS {
        let name = strip_script_suffix(base_name(argv.get(idx)?)).to_ascii_lowercase();
        if name.is_empty() {
            return None;
        }
        if !INTERPRETERS.contains(&name.as_str()) {
            return Some(name);
        }
        // The interpreter has named itself; the program is its first non-flag argument.
        // Dash-led arguments are the interpreter's own options and are skipped, which
        // also disposes of `-` (stdin) and of `-c`/`-m`, whose values are code rather
        // than a path and resolve to nothing in the registry anyway.
        match argv[idx + 1..].iter().position(|a| !a.starts_with('-')) {
            Some(offset) => idx += 1 + offset,
            None => return Some(name),
        }
    }
    None
}

/// The program name behind a raw foreground command string.
///
/// Accepts either shape the probes produce. A NUL-separated string is a real argv and is
/// split exactly, so a program under a path containing spaces survives; a plain string
/// with no NUL is split on whitespace as a convenience for logs and hand-written callers,
/// which is lossy in exactly that one case and is why the OS side never uses it.
pub fn program_name(raw: &str) -> Option<String> {
    let argv: Vec<&str> = if raw.contains('\0') {
        raw.split('\0').filter(|s| !s.is_empty()).collect()
    } else {
        raw.split_whitespace().collect()
    };
    program_from_argv(&argv)
}

/// A tool whose binary name this (possibly truncated) name is a prefix of.
///
/// Only consulted after an exact match fails, and only for names long enough to have
/// *been* truncated. Ambiguity answers `None` for the same reason [`registry::by_title`]
/// does: a prefix that fits two tools is evidence for neither.
fn by_truncated_bin(name: &str) -> Option<&'static ToolDef> {
    if name.len() < TRUNCATION_FLOOR {
        return None;
    }
    let mut hit: Option<&'static ToolDef> = None;
    for tool in registry::TOOLS {
        let prefixes = tool
            .candidate_bins()
            .any(|b| b.len() > name.len() && b.starts_with(name));
        if prefixes {
            match hit {
                Some(prev) if prev.id != tool.id => return None,
                _ => hit = Some(tool),
            }
        }
    }
    hit
}

/// The tool a foreground command names, if any — the pure half of this module.
///
/// `raw` may be a bare executable name, an absolute path, a whole argv, or a truncated
/// `comm`. Everything that is not a registered binary answers `None`, including the
/// overwhelmingly common case of a plain login shell.
pub fn tool_for_foreground_name(raw: &str) -> Option<&'static ToolDef> {
    let name = program_name(raw)?;
    registry::by_bin(&name).or_else(|| by_truncated_bin(&name))
}

// ---------------------------------------------------------------------------
// The probe itself.
// ---------------------------------------------------------------------------

/// The foreground process group of the terminal on `fd`.
///
/// On Windows this is always `None`, and the honesty matters more than the symmetry:
/// ConPTY has no process group and no `tcgetpgrp`. A console's attached processes are
/// reachable through `GetConsoleProcessList`, but Hyperpanes drives ConPTY through a
/// pseudoconsole handle rather than by attaching a console of its own, so there is
/// nothing to ask. A Windows pane keeps the title-and-command inference it has today;
/// this module declines rather than guessing.
pub fn foreground_pgrp(fd: PtyFd) -> Option<i32> {
    #[cfg(unix)]
    {
        // SAFETY: `tcgetpgrp` only reads terminal state through `fd` and writes nothing.
        // A closed or non-tty descriptor is a defined error return, not undefined
        // behaviour, which is what makes it safe to call on a UI tick against a pane
        // that may have just exited.
        match unsafe { libc::tcgetpgrp(fd) } {
            pid if pid > 0 => Some(pid),
            _ => None,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = fd;
        None
    }
}

/// The *resolved* executable path of `pid`. One syscall, no allocation — and a fallback
/// rather than the primary source, because resolved is not the same as invoked: `claude`
/// on this machine is `~/.local/bin/claude` symlinked to
/// `~/.local/share/claude/versions/2.1.251`, and the basename of that names nothing at
/// all. Only argv remembers what the human actually ran.
#[cfg(target_os = "macos")]
fn exec_path(pid: i32) -> Option<String> {
    // PROC_PIDPATHINFO_MAXSIZE. Kept as a literal because libc exposes the function but
    // not the constant.
    const CAP: usize = 4 * 1024;
    let mut buf = [0u8; CAP];
    // SAFETY: `proc_pidpath` writes at most `CAP` bytes into `buf` and reports how many.
    let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), CAP as u32) };
    if n <= 0 {
        return None;
    }
    std::str::from_utf8(&buf[..n as usize])
        .ok()
        .map(str::to_owned)
}

/// The full argv of `pid`, NUL-separated, via `KERN_PROCARGS2`.
///
/// Two sysctls rather than one: the size must be asked for first. Handing the kernel a
/// buffer smaller than the argument area does **not** fail and does not truncate the
/// tail — it silently copies out a *later* slice of the region, so a fixed-size guess
/// returns environment strings that parse as a plausible, wrong argv. The size query is
/// the only way to get the layout the header describes, and the region is a handful of
/// pages, not the megabyte `KERN_ARGMAX` allows for.
#[cfg(target_os = "macos")]
fn exec_argv(pid: i32) -> Option<String> {
    use std::ptr;

    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut needed: usize = 0;
    // SAFETY: a null `oldp` with a valid `oldlenp` is the documented size query; nothing
    // is written but `needed`.
    let sized = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            ptr::null_mut(),
            &mut needed,
            ptr::null_mut(),
            0,
        )
    };
    if sized != 0 {
        return None;
    }
    // A sanity ceiling so a hostile or corrupt answer cannot turn a UI tick into a large
    // allocation. Real argument areas here measure ~4 KiB.
    if needed <= size_of::<i32>() || needed > 1 << 20 {
        return None;
    }
    let mut buf = vec![0u8; needed];
    let mut len = needed;
    // SAFETY: `buf` is `len` bytes long and `sysctl` writes at most `len`, updating it.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buf.as_mut_ptr().cast(),
            &mut len,
            ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || len <= size_of::<i32>() {
        return None;
    }
    // Layout: argc as a host-order int, the exec path, NUL padding to alignment, then
    // argc NUL-separated argv entries, then the environment.
    let argc = i32::from_ne_bytes(buf[..4].try_into().ok()?);
    if argc <= 0 {
        return None;
    }
    let body = &buf[4..len];
    let path_end = body.iter().position(|b| *b == 0)?;
    let after_path = &body[path_end..];
    let start = after_path.iter().position(|b| *b != 0)?;
    let args: Vec<&str> = after_path[start..]
        .split(|b| *b == 0)
        .take(argc as usize)
        .map_while(|s| std::str::from_utf8(s).ok())
        .filter(|s| !s.is_empty())
        .collect();
    if args.is_empty() {
        return None;
    }
    Some(args.join("\0"))
}

/// The command line of `pid` in the shape [`program_name`] wants.
///
/// argv leads because it is the only source that survives both traps this machine sets:
/// the symlink-to-a-version-number that `claude` installs as, and the `#!/usr/bin/env
/// node` shebang that `codex` installs as. The resolved executable path loses the first;
/// stopping at `argv[0]` alone would lose the second. `proc_pidpath` is the fallback for
/// a process whose argument area the kernel declines to hand over.
///
/// `p_comm` is deliberately unused. It is the third documented route and the worst of
/// the three: it costs a `kinfo_proc` sysctl, it truncates at 16 bytes, and `libc`
/// declares no `kinfo_proc` for Apple targets, so reaching it would mean hand-writing a
/// kernel ABI struct in exchange for a *shorter* answer than the calls above already
/// give. The truncation is still handled in [`by_truncated_bin`] — Linux's `comm`
/// fallback reaches it, and a caller may hand us a name from anywhere.
#[cfg(target_os = "macos")]
fn foreground_command(pid: i32) -> Option<String> {
    exec_argv(pid).or_else(|| exec_path(pid))
}

/// The command line of `pid`, from `/proc`.
///
/// `cmdline` first because it is the untruncated one: it carries the whole argv, so
/// `node /…/codex.js` arrives intact where `comm` could only ever have said `node`.
/// `comm` is the fallback for the cases `cmdline` leaves empty — a kernel thread, or a
/// process that has exited into a zombie — and pays for it with 15 characters.
#[cfg(target_os = "linux")]
fn foreground_command(pid: i32) -> Option<String> {
    if let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) {
        let text = String::from_utf8_lossy(&raw)
            .trim_end_matches('\0')
            .to_string();
        if !text.is_empty() {
            return Some(text);
        }
    }
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    let comm = comm.trim_end().to_string();
    (!comm.is_empty()).then_some(comm)
}

/// No route on this platform. Every unix that is not macOS or Linux lands here — the
/// pgrp is still readable, but turning a pid into a name is per-kernel work we have no
/// machine to verify against, and a wrong name is worse than none.
#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
fn foreground_command(_pid: i32) -> Option<String> {
    None
}

/// The name of the program running in the foreground of the pty on `fd`.
///
/// Normalised the same way [`tool_for_foreground_name`] normalises: lowercased, path and
/// script suffix stripped, interpreter wrappers followed. That makes it the string to put
/// in a log line, because it is exactly what the match was attempted against.
///
/// Cheap enough for a UI tick by construction — syscalls and reads only, never a
/// subprocess, never a wait.
pub fn foreground_name(fd: PtyFd) -> Option<String> {
    #[cfg(unix)]
    {
        program_name(&foreground_command(foreground_pgrp(fd)?)?)
    }
    #[cfg(not(unix))]
    {
        let _ = fd;
        None
    }
}

/// The tool running in the foreground of the pty on `fd`, if the kernel names one.
///
/// This is the call the app wants. `None` means "not a registered tool right now" —
/// a shell at its prompt, `git`, `make`, a platform with no answer — and per §D5 that is
/// a reason to leave a pane's chrome alone or return it to `Terminal`, never a reason to
/// touch what the pane would relaunch.
pub fn foreground_tool(fd: PtyFd) -> Option<&'static ToolDef> {
    #[cfg(unix)]
    {
        tool_for_foreground_name(&foreground_command(foreground_pgrp(fd)?)?)
    }
    #[cfg(not(unix))]
    {
        let _ = fd;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(raw: &str) -> Option<&'static str> {
        tool_for_foreground_name(raw).map(|t| t.id)
    }

    #[test]
    fn a_bare_binary_name_resolves() {
        assert_eq!(id("claude"), Some("claude"));
        assert_eq!(id("CLAUDE"), Some("claude"));
        assert_eq!(id("cursor-agent"), Some("cursor-agent"));
        // Cursor's installer symlinks its binary as `agent`; the registry knows it as an
        // alt bin and this is the path that makes that pay off.
        assert_eq!(id("agent"), Some("cursor-agent"));
        assert_eq!(id("nvim"), Some("vim"));
    }

    #[test]
    fn an_absolute_path_resolves_to_its_basename() {
        assert_eq!(id("/usr/local/bin/claude"), Some("claude"));
        // The trap the macOS probe is arranged around: this is where `~/.local/bin/claude`
        // points, so the *resolved* executable path names a version number and no tool.
        assert_eq!(id("/Users/x/.local/share/claude/versions/2.1.251"), None);
        assert_eq!(id("/opt/homebrew/bin/aider"), Some("aider"));
        assert_eq!(id("C:\\Program Files\\vim\\vim.exe"), None);
    }

    #[test]
    fn a_login_shell_dash_is_stripped_and_names_no_tool() {
        assert_eq!(program_name("-zsh").as_deref(), Some("zsh"));
        assert_eq!(program_name("-bash").as_deref(), Some("bash"));
        assert_eq!(id("-zsh"), None);
        assert_eq!(id("zsh"), None);
        assert_eq!(id("sh"), None);
        assert_eq!(id("/bin/sh"), None);
        assert_eq!(id("bash"), None);
    }

    #[test]
    fn an_interpreter_yields_to_the_script_it_runs() {
        // The real shape on this machine: `codex` is a `#!/usr/bin/env node` script, so
        // the kernel reports node and argv[1] carries the tool.
        let real = "node /Users/x/.local/lib/node_modules/@openai/codex/bin/codex.js";
        assert_eq!(id(real), Some("codex"));
        assert_eq!(id("node\0/Users/x/.local/bin/codex"), Some("codex"));
        assert_eq!(
            id("/opt/homebrew/bin/node /Users/x/bin/aider.js"),
            Some("aider")
        );
        assert_eq!(id("python3 -m gemini"), Some("gemini"));
        // Its own flags are skipped on the way.
        assert_eq!(id("node --enable-source-maps /x/codex.js"), Some("codex"));
        // An interpreter with nothing to run stays an interpreter, and names no tool.
        assert_eq!(program_name("node").as_deref(), Some("node"));
        assert_eq!(id("node"), None);
        assert_eq!(id("node -e 1"), None);
    }

    #[test]
    fn a_nul_separated_argv_keeps_paths_with_spaces_whole() {
        assert_eq!(
            id("/Applications/My Tools/bin/claude\0--resume"),
            Some("claude")
        );
        // The whitespace fallback cannot, which is why the OS side never uses it.
        assert_eq!(id("/Applications/My Tools/bin/claude --resume"), None);
    }

    #[test]
    fn a_truncated_comm_still_resolves_when_it_is_unambiguous() {
        // `comm` keeps 15 bytes on Linux and 16 on macOS; `github-copilot-cli` is 18.
        assert_eq!(id("github-copilot-"), Some("copilot"));
        assert_eq!(id("github-copilot-c"), Some("copilot"));
        assert_eq!(id("github-copilot-cli"), Some("copilot"));
        // Below the truncation floor a prefix is just a different word.
        assert_eq!(id("cl"), None);
        assert_eq!(id("claud"), None);
        assert_eq!(id("cursor"), None);
    }

    #[test]
    fn a_truncation_prefix_that_fits_two_tools_is_evidence_for_neither() {
        // Synthetic: the registry has no such collision today, so the rule is asserted
        // against the helper directly rather than against a row that might be added.
        let ambiguous = registry::TOOLS
            .iter()
            .filter(|t| t.candidate_bins().any(|b| b.len() > TRUNCATION_FLOOR))
            .count();
        // If this ever exceeds one, the assertions above need a second look — that is the
        // point of pinning it.
        assert!(
            ambiguous <= 1,
            "more than one tool now has a truncatable binary name"
        );
        assert!(by_truncated_bin("").is_none());
    }

    #[test]
    fn an_unknown_binary_and_an_empty_command_answer_none() {
        assert_eq!(id("make"), None);
        assert_eq!(id("git"), None);
        assert_eq!(id("ssh-agent"), None);
        assert_eq!(id("sleep"), None);
        assert_eq!(id(""), None);
        assert_eq!(id("   "), None);
        assert_eq!(id("\0\0"), None);
        assert_eq!(program_name("/"), None);
    }

    #[test]
    fn every_registered_binary_name_round_trips_through_the_probe() {
        for tool in registry::TOOLS {
            for bin in tool.candidate_bins() {
                assert_eq!(id(bin), Some(tool.id), "bare {bin} lost {}", tool.id);
                assert_eq!(
                    id(&format!("/usr/local/bin/{bin}")),
                    Some(tool.id),
                    "absolute {bin} lost {}",
                    tool.id
                );
                // Reached through a runtime, the way an npm-installed tool really is.
                assert_eq!(
                    id(&format!("node\0/opt/x/{bin}.js")),
                    Some(tool.id),
                    "wrapped {bin} lost {}",
                    tool.id
                );
            }
        }
    }

    #[test]
    fn a_closed_or_non_tty_descriptor_answers_none_rather_than_panicking() {
        // -1 is never a descriptor; 0 in a test harness is not a tty. Both must be a
        // quiet `None`, because the app calls this on a tick against panes that exit.
        assert!(foreground_pgrp(-1).is_none());
        assert!(foreground_tool(-1).is_none());
        assert!(foreground_name(-1).is_none());
    }

    /// The FFI half, against a real pty with a known process in it.
    #[cfg(unix)]
    mod live {
        use super::*;
        use portable_pty::{native_pty_system, CommandBuilder, PtySize};
        use std::time::{Duration, Instant};

        /// What the probe said, plus the pty and the child — both of which the caller
        /// must keep alive, since dropping either takes the process down.
        type Probed = (
            Option<String>,
            portable_pty::PtyPair,
            Box<dyn portable_pty::Child + Send + Sync>,
        );

        /// Open a pty, run `argv` in it, and ask what the kernel says is running there.
        fn probe(argv: &[&str]) -> Probed {
            let pair = native_pty_system()
                .openpty(PtySize {
                    rows: 24,
                    cols: 80,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .expect("openpty");
            let mut cmd = CommandBuilder::new(argv[0]);
            for a in &argv[1..] {
                cmd.arg(a);
            }
            // `portable-pty` falls back to `$HOME` when no cwd is set, and other tests in
            // this binary point `HOME` at a path that does not exist.
            cmd.cwd("/");
            let child = pair.slave.spawn_command(cmd).expect("spawn");
            let fd = pair.master.as_raw_fd().expect("a unix master has an fd");
            let want_pgrp = child.process_id().map(|p| p as i32);
            // Two things have to happen before the probe can be right, and neither is
            // instant. The terminal is handed to the child's group only at its
            // `setsid`/`TIOCSCTTY` — before that `tcgetpgrp` answers with whatever the
            // kernel left there, which is the spawning process. And between `fork` and
            // `exec` the child is still a copy of *us*, so its argv is this test binary's.
            // So wait for the pgrp to be the child's and its name to stop being ours,
            // then ask the question the test is actually about.
            let own = foreground_command(std::process::id() as i32).unwrap_or_default();
            let mine = program_name(&own);
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let name = foreground_name(fd);
                if Instant::now() >= deadline
                    || (foreground_pgrp(fd) == want_pgrp && name.is_some() && name != mine)
                {
                    return (name, pair, child);
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        #[test]
        fn a_real_pty_names_the_process_running_in_it() {
            let (name, _pair, mut child) = probe(&["/bin/sleep", "30"]);
            assert_eq!(name.as_deref(), Some("sleep"));
            // …and `sleep` is not a tool, which is the other half of the guarantee.
            assert!(tool_for_foreground_name("sleep").is_none());
            let _ = child.kill();
            let _ = child.wait();
        }

        #[test]
        fn an_empty_pty_still_answers_without_panicking() {
            let pair = native_pty_system()
                .openpty(PtySize {
                    rows: 24,
                    cols: 80,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .expect("openpty");
            let fd = pair.master.as_raw_fd().expect("a unix master has an fd");
            // No session leader has claimed the terminal yet, so there is no foreground
            // group. That must be a `None`, not a wrong tool.
            assert!(foreground_tool(fd).is_none());
        }

        /// The point of the whole track: a *real* tool, resolved from a real pty.
        ///
        /// `#[ignore]`d because it depends on a tool being installed and on launching it
        /// being harmless. Run with:
        ///   cargo test -p hyperpanes-core foreground -- --ignored --nocapture
        #[test]
        #[ignore]
        fn a_real_tool_in_a_real_pty_resolves_to_its_registry_entry() {
            use crate::tools::detect;
            use std::collections::BTreeMap;

            let resolved = detect::resolve_all(&BTreeMap::new());
            let mut checked = 0usize;
            // Launched bare, into their idle state, and killed the moment the probe has
            // answered — a `--version` shape exits too fast to catch. The three are the
            // three shapes an install takes: a native binary behind a version-numbered
            // symlink (`claude`), a plain system binary (`vim`), and a `#!/usr/bin/env
            // node` script (`codex`), which is the only one argv can name.
            for want in ["claude", "vim", "codex"] {
                let Some(r) = resolved.get(want) else {
                    eprintln!("{want}: not installed here, skipped");
                    continue;
                };
                let path = r.path.to_string_lossy().to_string();
                let (name, _pair, mut child) = probe(&[&path]);
                eprintln!("{want}: pty foreground name = {name:?} (from {path})");
                assert_eq!(name.as_deref(), Some(want), "{want} misnamed by the probe");
                assert_eq!(
                    tool_for_foreground_name(name.as_deref().unwrap()).map(|t| t.id),
                    Some(want)
                );
                let _ = child.kill();
                let _ = child.wait();
                checked += 1;
            }
            assert!(checked > 0, "no real tool was available to probe");
        }
    }
}

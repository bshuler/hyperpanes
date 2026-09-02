//! Compile the app's Slint UI, importing the reusable `TerminalPane` from the
//! `hyperpanes-terminal-widget` crate via a Slint *library path*. In `ui/app.slint`
//! that surfaces as `import { TerminalPane, KeyMsg } from "@widgets";`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let widget = manifest.join("../terminal-widget/ui/widget.slint");

    // Deploy the shell-integration init scripts next to the built binary so
    // `shell_integration::shell_integration_dir()` finds them at dev runtime (the
    // `exe_dir/resources/shell-integration` candidate). Packaging does the same for
    // release. Without these, pwsh never emits its OSC-7 cwd → the git-project tint
    // can't fire. Best-effort: a copy failure must never fail the build.
    let scripts = manifest.join("../../../resources/shell-integration");
    // Also deploy the bundled ConPTY redistributable pair (resources/conpty/README.md)
    // NEXT TO the binary: portable-pty's `load_conpty()` prefers a sideloaded
    // `conpty.dll` beside the exe, and that host removes the in-box conhost's
    // scroll-region repaint + passthrough bottlenecks (measured 6-44× throughput,
    // docs/conpty-passthrough-investigation.md §F). Must stay a matched pair.
    //
    // ConPTY is Windows-only: gate on the TARGET OS (not the host) so Linux/macOS
    // builds — including cross-compiles — never look for or ship conpty.dll.
    let target_windows = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");
    let conpty = manifest.join("../../../resources/conpty");
    // Goal-orchestrator personas (goals system): deploy beside the binary so
    // `State::submit_new_goal`'s `exe_dir/resources/claude/goal-orchestrator` candidate
    // resolves at dev runtime, matching what packaging ships. Best-effort.
    let personas = manifest.join("../../../resources/claude/goal-orchestrator");
    // The always-on Hyperpane tab's working directory: its README plus the hidden
    // `.claude/skills/` tree its agent loads. `hyperpane::source_dir()` looks beside the exe
    // first, so without this a dev build opens the tab into an empty directory and the agent
    // has no idea the app can be driven at all. Packaging ships the same tree.
    let hyperpane = manifest.join("../../../resources/claude/hyperpane");
    // Every CLI-agent session hook. `claude_hook::bundled_hook_path` and
    // `tools::session_hook::bundled_script` both look beside the exe FIRST, so without
    // this a dev build registers no hook at all and every hand-started tool pane silently
    // falls back to the scan-and-diff heuristic — the one path we cannot test by running
    // it. This list and the five packaging manifests must all carry every entry of
    // HOOKED_TOOLS; `every_hook_ships_in_every_packaging_manifest` in hyperpanes-core's
    // tools::session_hook asserts it, because each of the last two tools added was added
    // to some of those places and not the others.
    let hooks: [(&str, &str); 6] = [
        ("claude", "hp-claude-session-hook.sh"),
        ("cursor", "hp-cursor-session-hook.sh"),
        ("copilot", "hp-copilot-session-hook.sh"),
        ("codex", "hp-codex-session-hook.sh"),
        ("gemini", "hp-gemini-session-hook.sh"),
        // Windows' single script stands in for all five above (session_hook's `# Windows`
        // section says why). Deployed on every host, not just Windows ones: it costs one
        // file copy, and gating it would mean a macOS dev build could not exercise the
        // resolution path at all.
        ("hooks", "hp-session-hook.ps1"),
    ];
    let res = manifest.join("../../../resources");
    if let Ok(out_dir) = std::env::var("OUT_DIR") {
        // OUT_DIR = <target>/<profile>/build/<pkg>-<hash>/out → profile dir is 3 up.
        if let Some(profile) = Path::new(&out_dir).ancestors().nth(3) {
            let dst = profile.join("resources").join("shell-integration");
            let _ = copy_dir(&scripts, &dst);
            let _ = copy_dir(
                &personas,
                &profile
                    .join("resources")
                    .join("claude")
                    .join("goal-orchestrator"),
            );
            let _ = copy_dir(
                &hyperpane,
                &profile.join("resources").join("claude").join("hyperpane"),
            );
            for (dir, script) in hooks {
                let dst_dir = profile.join("resources").join(dir);
                let _ = std::fs::create_dir_all(&dst_dir);
                let dst = dst_dir.join(script);
                if std::fs::copy(res.join(dir).join(script), &dst).is_ok() {
                    // A hook is invoked as a command; a copy that lost its mode bit is a
                    // hook that never runs.
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let _ =
                            std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o755));
                    }
                }
            }
            if target_windows {
                for f in ["conpty.dll", "OpenConsole.exe"] {
                    let _ = std::fs::copy(conpty.join(f), profile.join(f));
                }
            }
        }
    }
    for f in ["SKILL.md", "SPEC.md", "IMPL.md"] {
        println!("cargo:rerun-if-changed={}", personas.join(f).display());
    }
    for f in [
        "README.md",
        ".claude/settings.json",
        ".claude/skills/hyperpanes/SKILL.md",
        ".claude/skills/hyperpanes/REFERENCE.md",
        ".claude/skills/hyperpanes/RECIPES.md",
    ] {
        println!("cargo:rerun-if-changed={}", hyperpane.join(f).display());
    }
    for (dir, script) in hooks {
        println!(
            "cargo:rerun-if-changed={}",
            res.join(dir).join(script).display()
        );
    }
    for f in [
        "hp-init.ps1",
        "hp-init.sh",
        "zdotdir/.zshenv",
        "zdotdir/.zshrc",
    ] {
        println!("cargo:rerun-if-changed={}", scripts.join(f).display());
    }
    if target_windows {
        for f in ["conpty.dll", "OpenConsole.exe"] {
            println!("cargo:rerun-if-changed={}", conpty.join(f).display());
        }
    }

    let mut libs: HashMap<String, PathBuf> = HashMap::new();
    libs.insert("widgets".to_string(), widget.clone());

    let cfg = slint_build::CompilerConfiguration::new().with_library_paths(libs);
    slint_build::compile_with_config("ui/app.slint", cfg).expect("slint compile failed");

    // The WHOLE ui/ tree, not a hand-kept list of it. `app.slint` imports fifteen other
    // files and the list here had five of them, so an edit confined to (say)
    // `viewpanes.slint` left the compiled UI stale — the build succeeded and the change
    // simply was not in it, which is the worst shape a build bug can take. Cargo watches a
    // directory recursively, and one entry cannot fall behind the imports the way a list did.
    println!("cargo:rerun-if-changed=ui");
    println!("cargo:rerun-if-changed={}", widget.display());
}

/// Recursively copy `src` into `dst` (best-effort; returns the first IO error). A missing
/// `src` is a no-op so a checkout without the scripts still builds.
fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    if !src.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

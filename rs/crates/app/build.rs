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
    // The three CLI-agent session hooks. `claude_hook::bundled_hook_path` and
    // `tools::session_hook::bundled_script` both look beside the exe FIRST, so without
    // this a dev build registers no hook at all and every hand-started tool pane silently
    // falls back to the scan-and-diff heuristic — the one path we cannot test by running
    // it. Packaging ships the same three; keep the two lists in step.
    let hooks: [(&str, &str); 3] = [
        ("claude", "hp-claude-session-hook.sh"),
        ("cursor", "hp-cursor-session-hook.sh"),
        ("copilot", "hp-copilot-session-hook.sh"),
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

    for f in [
        "ui/app.slint",
        "ui/topbar.slint",
        "ui/paneview.slint",
        "ui/theme.slint",
        "ui/types.slint",
    ] {
        println!("cargo:rerun-if-changed={f}");
    }
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

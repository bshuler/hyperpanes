#!/usr/bin/env python3
"""Claude Code PreToolUse hook — refuse the commands that kill live panes.

Wired up in .claude/settings.json; reads the hook JSON on stdin. Exit 0 lets the
tool call through, exit 2 blocks it and hands the message on stderr back to the
model. Only Bash calls are inspected; everything else is allowed untouched.

What it blocks, and why each one is here
----------------------------------------
1. Anything that deletes or overwrites a bundle in /Applications. The session
   daemon runs out of the installed bundle and macOS kills a process whose
   executable is unlinked underneath it, so `rm -rf /Applications/Hyperpanes.app`
   destroys every program running in every pane — the exact thing the daemon
   exists to prevent. `scripts/install-macos.sh` is the sanctioned path and is
   allowed through.
2. Clearing the `uchg` lock that installer leaves behind. Unlocking is step one
   of doing (1) by hand.
3. Killing the session daemon. Same loss, one signal instead of one unlink. It
   is a legitimate thing for a person to want; it is not something to do on an
   agent's own initiative.
4. Stealing the keyboard: `osascript` that activates an app or synthesises
   keystrokes, `open -a`, and `hyperpanes ctl focus-pane`. There is a human
   typing into this machine, and yanking the front window out from under them
   mid-sentence sends their keys somewhere they did not aim them.

This is deliberately a blunt instrument. When it blocks something that genuinely
needs doing, the answer is to ask the person at the keyboard, not to route
around it.
"""

import json
import re
import sys

# Anything that can unlink or clobber a path.
DESTRUCTIVE = {
    "rm", "rmdir", "unlink", "shred", "trash",
    "ditto", "cp", "mv", "rsync",
    "tar", "unzip", "ln",
}
SIGNALS = {"kill", "pkill", "killall"}

SANCTIONED = "scripts/install-macos.sh"

APPLICATIONS = re.compile(r"/Applications\b", re.I)
HYPERPANES = re.compile(r"hyperpanes", re.I)
DAEMONISH = re.compile(r"hyperpanes|session-daemon", re.I)
SYNTH_INPUT = re.compile(r"\bactivate\b|\bkeystroke\b|\bkey code\b|System Events", re.I)


SHELLS = {"bash", "sh", "zsh", "ksh", "dash"}
HEREDOC = re.compile(r"<<-?\s*[\"']?([A-Za-z_][A-Za-z0-9_]*)[\"']?")


def without_heredocs(command):
    """Drop heredoc bodies, which are data being written, not commands being run.

    Without this the guard refuses to let anyone write documentation about the
    commands it blocks — the doc that explains the rule quotes the rule. A body
    fed to a shell is the exception: there it really is a command.
    """
    lines = command.split("\n")
    kept, i = [], 0
    while i < len(lines):
        line = lines[i]
        kept.append(line)
        i += 1
        match = HEREDOC.search(line)
        if not match:
            continue
        first = line.split()[0].rsplit("/", 1)[-1] if line.split() else ""
        feeds_a_shell = first in SHELLS
        delimiter = match.group(1)
        while i < len(lines) and lines[i].strip() != delimiter:
            if feeds_a_shell:
                kept.append(lines[i])
            i += 1
        i += 1  # the delimiter line itself
    return "\n".join(kept)


def words(command):
    """Bare words of the command, quoting and paths stripped.

    Deliberately flat: no attempt to work out which word is "the" verb. A verb
    can hide behind `sudo`, behind `find -exec`, on the far side of a pipe from
    the path it will destroy, or inside `sh -c`. Matching any destructive word
    against any dangerous path in the same command costs the occasional false
    positive and closes all of those at once.
    """
    return {w.strip("\"\'`(){};,").rsplit("/", 1)[-1] for w in command.split()}


def refusal(command):
    """The reason this command is blocked, or None if it may proceed."""
    if SANCTIONED in command:
        return None

    command = without_heredocs(command)
    seen = words(command)
    hits_applications = bool(APPLICATIONS.search(command))

    destructive = sorted(seen & DESTRUCTIVE)
    if destructive and hits_applications:
        return (
            f"Blocked: `{destructive[0]}` against /Applications would unlink or overwrite an "
            "installed app bundle. The Hyperpanes session daemon runs out of that bundle, and "
            "macOS kills a process whose executable is removed underneath it — every program "
            f"in every pane dies with it. Use `{SANCTIONED}`, which renames the old bundle "
            "aside instead of deleting it and verifies the daemon survived."
        )
    if "chflags" in seen and "nouchg" in command and hits_applications:
        return (
            "Blocked: clearing the uchg lock on an installed bundle. That lock is what stops a "
            f"stray rm from ending the user's sessions. `{SANCTIONED}` clears and restores it "
            "as part of a safe swap."
        )
    if seen & SIGNALS and DAEMONISH.search(command):
        return (
            "Blocked: signalling Hyperpanes. If this is the session daemon it owns every "
            "pane's PTY, so killing it ends every running program in every pane. That is a "
            "decision for the person at the keyboard — ask, and let them run it."
        )
    if "osascript" in seen and SYNTH_INPUT.search(command):
        return (
            "Blocked: osascript that fronts an app or synthesises keystrokes. Someone is "
            "typing on this machine; taking the front window sends their keys into whatever "
            "you just raised. Ask them to drive the UI instead."
        )
    if "open" in seen and "-a" in command.split() and HYPERPANES.search(command):
        return (
            "Blocked: `open -a` fronts the app and takes the user's focus mid-keystroke. "
            "Launch the binary directly if you need a process, or ask them to open it."
        )
    if "focus-pane" in seen:
        return (
            "Blocked: `ctl focus-pane` moves the user's focus inside the app, so their next "
            "keystrokes land in a pane they did not choose. Read the pane instead "
            "(`ctl read`), which needs no focus."
        )
    return None


def main():
    try:
        event = json.load(sys.stdin)
    except (json.JSONDecodeError, ValueError):
        return 0  # A hook that cannot parse its input must not block the session.
    if event.get("tool_name") != "Bash":
        return 0
    reason = refusal(event.get("tool_input", {}).get("command", ""))
    if reason is None:
        return 0
    print(reason, file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main())

# Not killing the live session

The session daemon exists so that the programs running in panes survive a GUI
restart ([session-daemon-plan.md](session-daemon-plan.md)). It is a detached
process that owns every PTY; the GUI is only a client. Quitting the GUI, even
force-quitting it, leaves it alone — verified:

```sh
pgrep -fl -- --session-daemon    # still there after the GUI is gone
```

There is exactly one ordinary act that destroys it, and it does not look
dangerous:

```sh
rm -rf /Applications/Hyperpanes.app        # <- ends every program in every pane
```

The daemon is executing out of the installed bundle, and macOS kills a process
whose code-signed executable is unlinked underneath it. So the obvious install —
delete the old app, copy the new one in — silently defeats the entire feature.
The layout comes back on the next launch; the running processes do not.

That mistake has been made on this machine. What follows is the three layers put
in place so it cannot be made again by anyone, including whoever is confident it
will be fine this time.

## 1. A safe install path: `scripts/install-macos.sh`

```sh
bash rs/packaging/macos/bundle.sh 0.0.28      # build (~9 min)
bash scripts/install-macos.sh                 # install
```

It never deletes a bundle a process might still be running from:

| Step | Why |
|---|---|
| Record every `--session-daemon` pid | So "did anything die?" is a check, not a hope |
| `ditto` the new bundle to `/Applications/.Hyperpanes.app.incoming` | Staging on the same volume makes the final step a rename |
| Move the old bundle to `/Applications/.hyperpanes-attic/` | **Rename, never remove.** The inode survives, so the live daemon keeps running from the moved directory without noticing |
| `mv` the staged bundle into place | Atomic: no instant where the path is half-written |
| `chflags -R uchg` the result | Layer 2, below |
| Re-check every recorded pid | A daemon that died is a hard error, not a warning |
| Prune the attic | Only bundles with no live process, and never the newest two |

Afterwards the old daemon is still serving your panes, still running the
*previous* build out of the attic. That is correct and intended. It picks up the
new build only when it is deliberately restarted, and restarting it ends every
program in every pane — so the script prints the command rather than running it.

## 2. An OS-level lock the installer leaves behind

The installed bundle is locked with the BSD user-immutable flag:

```sh
chflags -R uchg /Applications/Hyperpanes.app
```

Every path to the accident then fails at the syscall, with no cooperation
required from whoever typed it:

| Command | Result |
|---|---|
| `rm -rf /Applications/Hyperpanes.app` | `Operation not permitted` |
| `ditto new /Applications/Hyperpanes.app` | `Operation not permitted` |
| `cp -R new/. /Applications/Hyperpanes.app/` | `Operation not permitted` |
| `rsync -a new/ /Applications/Hyperpanes.app/` | `Operation not permitted` |
| `mv /Applications/Hyperpanes.app elsewhere` | `Operation not permitted` |

`sudo` does not help; the flag has to be cleared first, deliberately, which is
the point. `scripts/install-macos.sh` clears it and restores it as part of the
swap. Nothing else should ever clear it.

The lock does not disturb a running process — an app bundle is read-only in
normal operation, and applying it to the live bundle left both the daemon and
the GUI running.

## 3. A hook that refuses the command before it runs

`scripts/guard-live-sessions.py` is a Claude Code `PreToolUse` hook, wired up in
[`.claude/settings.json`](../.claude/settings.json). It inspects every `Bash`
command and exits 2 — which blocks the call and hands the reason back — for:

- any destructive word (`rm`, `ditto`, `cp`, `mv`, `rsync`, `tar`, …) in a
  command that also names `/Applications`;
- `chflags … nouchg` against `/Applications`, which is step one of doing it by
  hand;
- `kill` / `pkill` / `killall` aimed at `hyperpanes` or `--session-daemon`;
- `osascript` that activates an app or synthesises keystrokes, `open -a`, and
  `ctl focus-pane` — all of which take the keyboard away from whoever is typing.

Commands containing `scripts/install-macos.sh` pass through: that is the
sanctioned path.

A heredoc body is treated as data, not as commands — otherwise the guard would
refuse to let anyone write this very document, which quotes every command it
blocks. The exception is a body being fed to a shell (`bash <<EOF`), where it
really is a command.

The matching is deliberately blunt — any dangerous word against any dangerous
path, no attempt to parse shell grammar — because a verb can hide behind `sudo`,
behind `find -exec`, on the far side of a pipe, or inside `sh -c`. It will
occasionally refuse something harmless. The table test says what it does:

```sh
bash scripts/guard-live-sessions.test.sh
```

Keep the allow cases honest when adding rules. A guard that blocks routine work
gets switched off, and then it guards nothing.

## Both are tested, and the tests run in CI

```sh
bash scripts/guard-live-sessions.test.sh   # allow/deny table for the hook
bash scripts/install-macos.test.sh         # end-to-end swap against a throwaway --dest
```

The installer test asserts the property that matters rather than reading the
script for it: it starts a real process out of the installed bundle, installs
four more builds over the top, and requires that the process is still alive at
the end. `test.yml`'s `guardrails` job runs both on `macos-latest`, blocking.

## Why three layers

They fail differently. The installer is only used by someone who knows it
exists; the hook only applies to a Claude Code session with that settings file
loaded; the `uchg` flag applies to every shell on the machine but can be cleared
by anyone determined to. Together, the accident needs a person to clear a flag
on purpose, in a session where a hook told them not to, instead of a single
plausible-looking line.

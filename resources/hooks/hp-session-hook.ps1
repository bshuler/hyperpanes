<#
  Windows session hook for every CLI agent hyperpanes knows -> pane->conversation map.

  The POSIX hooks are five separate `resources/<tool>/hp-<tool>-session-hook.sh` scripts.
  This is one script for all five, because on Windows the per-tool part is four lines of a
  switch and the *rest* — draining stdin, finding %APPDATA%, writing the marker atomically
  without a UTF-8 BOM — is identical and is the part that is easy to get subtly wrong. Five
  copies of it would be five places to fix each time.

  It is registered as, with the same nested/flat shape each tool takes:

    powershell -NoProfile -ExecutionPolicy Bypass -File "<this script>" -Tool <tool id>

  `-ExecutionPolicy Bypass` because a hook is not an interactive script the human chose to
  run: on a default Windows install the policy is `Restricted`, and without this the hook is
  refused before its first line and never fires — silently, since nothing surfaces a hook's
  exit status. `-NoProfile` so a user profile cannot slow down or break a script that runs on
  every session start.

  Per tool, the payload shape (each read off a real install of that tool — see the header of
  the matching .sh for what was run and against which version):

    claude       session_id      / cwd               / hook_event_name == "SessionEnd"
    cursor-agent conversation_id / workspace_roots[0]/ hook_event_name == "sessionEnd"
    copilot      sessionId       / cwd               / the presence of "reason"
    codex        session_id      / cwd               / hook_event_name == "SessionEnd"
    gemini       session_id      / cwd               / hook_event_name == "SessionEnd"

  Comparisons are `-ceq`, not `-eq`. PowerShell's `-eq` is case-INSENSITIVE, which would make
  claude's `SessionEnd` and cursor's `sessionEnd` the same string and quietly erase the
  distinction the .sh scripts draw — a difference that is load-bearing, since a tool that
  spells its start event with the other casing must not be read as an end.

  Marker path must mirror hyperpanes-core tools::session_hook::marker_dir, which on Windows
  is %APPDATA%\hyperpanes\tool-sessions\<tool>\<pane-id>.json — claude alone predates that
  layout and keeps %APPDATA%\hyperpanes\claude-sessions\<pane-id>.json.

  Outside a hyperpanes pane, or on any error at all, this exits 0 having done nothing: a
  hook that fails must never take the agent down with it.
#>
param([Parameter(Mandatory = $true)][string]$Tool)

# Drain stdin unconditionally and first. The agent writes the payload whether or not we
# want it, and a hook that exits without reading hands it a broken pipe.
$raw = ''
try { $raw = [Console]::In.ReadToEnd() } catch { }

$pane = $env:HYPERPANES_PANE_ID
if ([string]::IsNullOrEmpty($pane)) { exit 0 }
# The pane id becomes a filename, so it may not steer one. Same gate as read_pane_mark's.
if ($pane -match '[\\/]' -or $pane.Contains('..')) { exit 0 }

$appdata = $env:APPDATA
if ([string]::IsNullOrEmpty($appdata)) { exit 0 }

try {
    $data = $raw | ConvertFrom-Json
} catch {
    exit 0
}
if ($null -eq $data) { exit 0 }

# A property missing from the payload must read as absent, not throw — ConvertFrom-Json
# returns a PSCustomObject that simply has no such member.
function Get-Field($obj, [string]$name) {
    if ($null -eq $obj -or $null -eq $obj.PSObject.Properties[$name]) { return $null }
    return $obj.PSObject.Properties[$name].Value
}

$sub = @('tool-sessions', $Tool)
switch ($Tool) {
    'claude' {
        $sub     = @('claude-sessions')
        $id      = Get-Field $data 'session_id'
        $cwd     = Get-Field $data 'cwd'
        $isEnd   = ((Get-Field $data 'hook_event_name') -ceq 'SessionEnd')
        # configDir records the account this conversation was saved under: `claude` stores
        # transcripts in $CLAUDE_CONFIG_DIR\projects, so a relaunch must set the SAME
        # CLAUDE_CONFIG_DIR for `claude --resume <id>` to find it (multi-account).
        $extra   = @{ configDir = [string]$env:CLAUDE_CONFIG_DIR }
    }
    'cursor-agent' {
        $id      = Get-Field $data 'conversation_id'
        # No `cwd` field: resume is directory-scoped, and workspace_roots[0] is the directory
        # cursor-agent itself considers the conversation to belong to.
        # `@(...)` because a PowerShell function RETURNING a one-element array yields the
        # element instead — so a payload with exactly one workspace root, which is the
        # normal case, arrives here as a bare string. Re-wrapping makes both shapes an array.
        $roots   = @(Get-Field $data 'workspace_roots')
        $cwd     = if ($roots.Count -gt 0 -and $roots[0] -is [string]) { $roots[0] } else { '' }
        $isEnd   = ((Get-Field $data 'hook_event_name') -ceq 'sessionEnd')
    }
    'copilot' {
        $id      = Get-Field $data 'sessionId'
        $cwd     = Get-Field $data 'cwd'
        # Neither copilot payload carries an event name, so the two are told apart by the
        # field only the end one has.
        $isEnd   = ($null -ne $data.PSObject.Properties['reason'])
    }
    default {
        # codex, gemini, and any future tool that took Claude Code's payload wholesale.
        $id      = Get-Field $data 'session_id'
        $cwd     = Get-Field $data 'cwd'
        $isEnd   = ((Get-Field $data 'hook_event_name') -ceq 'SessionEnd')
    }
}

# Joined a segment at a time rather than with a literal separator in the string, so the
# separator is always the platform's own — the script is Windows-only in practice, but it is
# exercised on macOS, and a literal `\` there becomes part of a filename instead.
$dir = Join-Path $appdata 'hyperpanes'
foreach ($seg in $sub) { $dir = Join-Path $dir $seg }
$path = Join-Path $dir "$pane.json"

try {
    if ($isEnd) {
        # -ErrorAction so an already-absent marker is not an error; SessionEnd can arrive
        # for a session whose start we never saw.
        Remove-Item -LiteralPath $path -Force -ErrorAction SilentlyContinue
    } else {
        New-Item -ItemType Directory -Path $dir -Force -ErrorAction Stop | Out-Null
        $marker = [ordered]@{ sessionId = [string]$id; cwd = [string]$cwd }
        if ($null -ne $extra) { foreach ($k in $extra.Keys) { $marker[$k] = $extra[$k] } }
        $json = $marker | ConvertTo-Json -Compress
        $tmp  = "$path.tmp"
        # WriteAllText with an explicit BOM-less UTF8Encoding, NOT Set-Content -Encoding utf8:
        # Windows PowerShell 5.1 writes a BOM for the latter, and serde_json rejects a
        # leading BOM — the marker would parse on the writer's side and be invisible to the
        # reader's.
        [System.IO.File]::WriteAllText($tmp, $json, (New-Object System.Text.UTF8Encoding($false)))
        # MOVEFILE_REPLACE_EXISTING underneath: atomic on one volume, so a reader polling
        # this path never sees a half-written marker.
        Move-Item -LiteralPath $tmp -Destination $path -Force -ErrorAction Stop
    }
} catch {
    # Deliberately swallowed, as in the .sh scripts.
}
exit 0

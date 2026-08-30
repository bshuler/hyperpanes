# macOS packaging — Hyperpanes.app + dmg

`bundle.sh <version>` (version without a leading `v`) builds `rs/crates/app`
in release mode, assembles `Hyperpanes.app`, and emits
`rs/packaging/out/hyperpanes-<version>.dmg` containing the app plus an
`/Applications` symlink. It runs from any cwd and works both on the Mac mini
and on a GitHub `macos-latest` (arm64) runner — only stock macOS tools are
used (`cargo`, `sips`, `iconutil`, `hdiutil`, `plutil`, `codesign`).

## Bundle layout

```
Hyperpanes.app/
  Contents/
    Info.plist                      # com.hyperpanes.app, the NS*UsageDescription
                                    # strings, .hyperpanes doc type + exported UTI
    _CodeSignature/                 # written by codesign; seals everything below
    MacOS/
      hyperpanes                    # the release binary
      resources -> ../Resources     # symlink, so the app's exe-relative lookup
                                    # and the bundle-idiomatic one land together
    Resources/
      hyperpanes.icns               # generated from build/icon.png via sips + iconutil
      shell-integration/            # hp-init.ps1 / hp-init.sh / zdotdir
      claude/goal-orchestrator/     # SKILL.md, SPEC.md, IMPL.md
```

Two lookups have to agree here. `core::shell_integration::shell_integration_dir()`
and `submit_new_goal()` both resolve `exe_dir/resources/...`, and in a bundle
`exe_dir` is `Contents/MacOS/`; the idiomatic bundle location is
`Contents/Resources/...`. That used to be answered by shipping two copies, which
signing forbids: `codesign` treats every directory under `Contents/MacOS` as
nested code and refuses to sign a bundle whose `Contents/MacOS` holds a
`SKILL.md`. So the files live in `Contents/Resources`, where the signature seals
them, and `Contents/MacOS/resources` is a relative symlink onto it — sealed as a
symlink, never descended into. One set of bytes, both lookups satisfied.

## Signing and notarization

Both are opt-in through the environment, and both degrade to a warning rather
than a failure, so `bundle.sh` stays runnable on a machine with no certificate:

| Variable | Effect |
|---|---|
| `HYPERPANES_SIGN_ID` | `codesign` identity to use. Unset → the first `Developer ID Application` in the login keychain; none → ad-hoc (`-`) with a loud warning. |
| `HYPERPANES_NOTARY_PROFILE` | An `xcrun notarytool store-credentials` profile name. Unset → notarization is skipped. Also skipped on an ad-hoc signature, which Apple will not notarize. |

No password ever reaches this script. Notarization credentials are created out
of band, once, with `xcrun notarytool store-credentials <profile-name>`, and
live in the keychain; `bundle.sh` refuses to run if an Apple ID or
app-specific password has been exported into the environment in the hope that
it would be picked up.

The app is signed with the hardened runtime and `entitlements.plist`, which
claims exactly two exceptions — `com.apple.security.device.audio-input` and
`com.apple.security.automation.apple-events`. Both are gates the hardened
runtime closes before TCC gets to ask; the reasoning, including why the JIT and
library-validation exceptions are deliberately absent, is in `bundle.sh` beside
the `ENTITLEMENTS` assignment. The file itself carries no XML comments because
AMFI, the parser `codesign` hands it to, rejects them.

Why it matters beyond Gatekeeper: macOS keys TCC grants to the signing
identity, so a Screen Recording or Full Disk Access grant made against an
ad-hoc signature is dropped the next time the app is rebuilt. A Developer ID
build keeps them.

### Installing an ad-hoc build (Gatekeeper)

An ad-hoc-signed, un-notarized dmg is quarantined on download, and a plain
double-click shows "Hyperpanes is damaged" or "cannot be opened because the
developer cannot be verified". Either of these gets past it:

- **Right-click → Open**: after copying `Hyperpanes.app` to `/Applications`,
  right-click (or Ctrl-click) the app → **Open** → **Open** in the dialog.
  Only needed once; afterwards it launches normally.
- **Strip the quarantine attribute** (Terminal):

  ```sh
  xattr -dr com.apple.quarantine /Applications/Hyperpanes.app
  ```

On newer macOS the first launch may instead be blocked outright with no Open
override; then use System Settings → Privacy & Security → "Open Anyway", or
the `xattr` command above.

## `.hyperpanes` file association

`Info.plist` declares the `com.hyperpanes.workspace` exported UTI (extension
`.hyperpanes`, conforms to `public.json`) and registers the app as its Owner
editor. LaunchServices picks the declaration up when the app is first copied
into `/Applications` (or launched). Double-clicking a `.hyperpanes` file then
opens it in the app — macOS passes the path as `argv[1]`, which flows through
the CLI's positional-path capture, same as the Windows `"%1"` association.

To verify a registration:

```sh
mdls -name kMDItemContentType some.hyperpanes
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -dump | grep -i hyperpanes
```

## Versioning

`CFBundleShortVersionString` carries the full `<version>` string;
`CFBundleVersion` gets the numeric prefix only (`0.1.0-test` → `0.1.0`)
because Apple requires period-separated numbers there.

The icon source is `build/icon.png` (512×512 — the same art as the Windows
`icon.ico`), so the iconset tops out at 512 px and omits the `512@2x` (1024)
slot; `iconutil` accepts that.

---
name: reinstall
description: Fully uninstall ConjureAlign from this Mac and reinstall it from the current worktree's code — removes every installed bundle (user and system) plus caches, builds a fresh release bundle from the checkout you are in, and installs CLAP + VST3 + AU. Use for "reinstall the plugin", "uninstall ConjureAlign", "install my current branch", "clean install", "get my changes into Logic/Ableton/REAPER".
user_invocable: true
---

# /reinstall — clean uninstall + reinstall from this worktree

Removes every trace of an installed ConjureAlign, then builds and installs the code of the
**current working directory** (worktree or main checkout). Run the whole thing from the
worktree root; never `cd` to the main checkout.

Tell the user up front: **quit every DAW first**. A host holding the dylib open will keep a
stale copy loaded, and on Logic the AU cache will not re-scan.

## Step 0 — Establish where you are

```bash
ROOT=$(git rev-parse --show-toplevel)
echo "Building from: $ROOT"
git -C "$ROOT" rev-parse --abbrev-ref HEAD
git -C "$ROOT" status --short
```

Report the branch to the user. If `status` is dirty, say so — the install will include the
uncommitted changes, which is usually the point, but the user should know.

## Step 1 — Uninstall

### 1a. User-domain bundles (no sudo)

```bash
rm -rf ~/Library/Audio/Plug-Ins/CLAP/ConjureAlign.clap \
       ~/Library/Audio/Plug-Ins/VST3/ConjureAlign.vst3 \
       ~/Library/Audio/Plug-Ins/Components/ConjureAlign.component
```

### 1b. System-domain bundles (installed by the .pkg — needs sudo, so the USER runs it)

Check first:

```bash
ls -d /Library/Audio/Plug-Ins/CLAP/ConjureAlign.clap /Library/Audio/Plug-Ins/VST3/ConjureAlign.vst3 /Library/Audio/Plug-Ins/Components/ConjureAlign.component 2>/dev/null
pkgutil --pkgs | grep -i conjure
```

If anything is listed, do **not** try to remove it yourself — you cannot supply a password.

Since 1.4.0 the .pkg ships its own uninstaller, which does all of this (both domains, the
receipts, and the AU cache) in one step. Prefer it when it is present — hand the user this
and wait for them to confirm, since it will prompt for their password:

```bash
"/Applications/ConjureDSP/Uninstall ConjureAlign.command" --yes --keep-settings
```

`--keep-settings` is deliberate here: this is an iteration cycle, not a clean-install test,
so the consent answers and device id should survive (step 1d is where that decision is
actually made). Note it also deletes itself when it finishes, which is expected.

If that file does not exist — an install from 1.3.0 or earlier — fall back to the manual
block and wait for the user to confirm they ran it:

```bash
sudo rm -rf /Library/Audio/Plug-Ins/CLAP/ConjureAlign.clap /Library/Audio/Plug-Ins/VST3/ConjureAlign.vst3 /Library/Audio/Plug-Ins/Components/ConjureAlign.component
```

Then forget the installer receipts it also lists (one `sudo pkgutil --forget <id>` per id from
the `grep` above). A leftover system bundle is the classic "my changes didn't show up" bug:
hosts scan `/Library` as well as `~/Library`, and Logic may pick the stale one.

### 1c. Caches

```bash
killall -9 AudioComponentRegistrar 2>/dev/null; rm -rf ~/Library/Caches/AudioUnitCache
```

The `;` is deliberate — `AudioComponentRegistrar` is on-demand, so `killall` exits non-zero
whenever it happens to be idle and `&&` would silently skip the rest.

### 1d. Install-wide settings (analytics/crash consent + device id)

This is what makes the uninstall *full*: the consent answer lives outside any DAW session, so
leaving it means the reinstalled plugin does not show the first-run privacy prompt.

```bash
ls -la ~/Library/Application\ Support/ConjureDSP/ConjureAlign/
```

Default is to remove it, since the user asked for a full uninstall — but say what that costs
(consent is re-asked, a new random device id is minted, so Mixpanel/Sentry see a new install)
and skip it if the user wants continuity, or if this is a quick iteration cycle rather than a
genuine clean-install test:

```bash
rm -rf ~/Library/Application\ Support/ConjureDSP/ConjureAlign
```

### 1e. Verify nothing is left

```bash
find ~/Library/Audio/Plug-Ins /Library/Audio/Plug-Ins -maxdepth 2 -iname 'ConjureAlign.*' 2>/dev/null
```

Empty output = uninstalled. Report the result before building.

## Step 2 — Build from THIS worktree

`cargo xtask bundle` is **unsafe in a `.claude/worktrees/` worktree**: nih_plug_xtask's
`chdir_workspace_root()` walks to the *topmost* ancestor holding a `Cargo.toml`, which from a
nested worktree is the main checkout — it will silently build and bundle whatever branch the
main checkout has on it. That has shipped a stale build before.

Detect and branch:

```bash
case "$ROOT" in
  */.claude/worktrees/*) echo "WORKTREE — use the symlink workaround" ;;
  *) echo "MAIN CHECKOUT — plain xtask is fine" ;;
esac
```

**Main checkout:**

```bash
cargo xtask bundle conjure_align --release
```

**Worktree** — build the xtask binary, then run it with `CARGO_MANIFEST_DIR` pointed at a
symlink to the worktree that lives *outside* the repo tree, so the ancestor walk cannot escape:

```bash
cargo build --release -p xtask
```

```bash
LINK=$(mktemp -d)/aa && ln -s "$ROOT" "$LINK" && CARGO_MANIFEST_DIR="$LINK" "$ROOT/target/release/xtask" bundle conjure_align --release
```

Bundles land in `$ROOT/target/bundled/`. **Verify they came from here and are fresh** — this
is the check that catches the worktree trap:

```bash
ls -ld "$ROOT"/target/bundled/ConjureAlign.{clap,vst3,component}
```

If any bundle is missing, or the timestamps predate the build you just ran, stop and tell the
user — do not install a bundle you cannot account for.

For a universal (Intel + Apple Silicon) build, substitute `bundle-universal` for `bundle` in
the command above. Only needed when the user asks; the default single-arch build is faster and
runs natively.

## Step 3 — Install

All three bundles, always. Bundling alone changes nothing a DAW loads.

```bash
cp -R "$ROOT"/target/bundled/ConjureAlign.clap ~/Library/Audio/Plug-Ins/CLAP/ && \
cp -R "$ROOT"/target/bundled/ConjureAlign.vst3 ~/Library/Audio/Plug-Ins/VST3/ && \
cp -R "$ROOT"/target/bundled/ConjureAlign.component ~/Library/Audio/Plug-Ins/Components/
```

The AU must live in a `Components` directory — no other location works.

Then force the AU re-scan (a rebuild at an unchanged version is otherwise served from cache):

```bash
killall -9 AudioComponentRegistrar 2>/dev/null; auval -v aufx ALGN CONJ 2>&1 | tail -30
```

A passing `auval` ends with `AU VALIDATION SUCCEEDED`. Expect these known-harmless warnings —
do not chase them: `MusicDeviceMIDIEvent … type 'aufx'`, missing Tail Time, no offline
rendering. Also confirm the channel-layout line reports **`[2, 2]  [1, 1]`** — if it shows only
`[2, 2]`, the vendored `deps/clap-wrapper-rs` patch is not in effect and the plugin will be
invisible on mono tracks in Logic.

Note what `auval` does *not* cover: it renders the main bus only (never the sidechain) and
loads the plugin in-process without building the Cocoa view, so a green `auval` says nothing
about the editor or AU sandboxing.

## Step 4 — Report

Tell the user, concisely:

- which branch/commit is now installed, and whether the tree was dirty
- what was removed (including anything they had to `sudo rm` themselves, and whether settings
  were cleared)
- the `auval` verdict, including the `[2, 2]  [1, 1]` line
- **restart the DAW** — and for Logic, confirm it validates under Settings → Plug-in Manager →
  ConjureDSP ("Reset & Rescan Selection" if it does not appear)
- if settings were cleared, that the first-run privacy prompt will appear again

Do not claim it works in a host; you have not opened one. Report what you verified.

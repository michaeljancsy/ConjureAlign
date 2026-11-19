---
name: reinstall
description: Fully uninstall ConjureAlign from this Mac and reinstall it from the current worktree's code — removes every installed bundle (user and system) plus caches, builds a fresh release bundle from the checkout you are in, and installs CLAP + VST3 + AU. Use for "reinstall the plugin", "uninstall ConjureAlign", "install my current branch", "clean install", "get my changes into Logic/Ableton/REAPER".
user_invocable: true
---

# /reinstall: clean uninstall, then install this checkout's build

Installs the code in the **current working directory** (worktree or main checkout). Run every
step from that directory; never `cd` to the main checkout.

DAW testing belongs on the test laptop (a GitHub issue brief to "TEST Conjure Test Runner"). Use
this skill on this Mac only when Michael asks for a local install.

## Step 0: Before you start

```bash
ROOT=$(git rev-parse --show-toplevel)
git -C "$ROOT" rev-parse --abbrev-ref HEAD
git -C "$ROOT" status --short
screen-turn status
```

- Tell the user the branch, and whether the tree has uncommitted changes (they will be included).
- Ask the user to quit every DAW. A running host keeps the old copy loaded.
- Claim the plug-in folders right before Step 1:
  `screen-turn claim --who "<your session title>" --minutes 30 --what "reinstall ConjureAlign"`.
  If someone else holds the turn, `screen-turn queue` and wait.

## Step 1: Uninstall

**User folders** (no password):

```bash
rm -rf ~/Library/Audio/Plug-Ins/CLAP/ConjureAlign.clap \
       ~/Library/Audio/Plug-Ins/VST3/ConjureAlign.vst3 \
       ~/Library/Audio/Plug-Ins/Components/ConjureAlign.component
```

**System folders** (installed by the .pkg; needs the user's password, so the user runs it):

```bash
ls -d /Library/Audio/Plug-Ins/{CLAP,VST3,Components}/ConjureAlign.* 2>/dev/null
pkgutil --pkgs | grep -i conjure
```

If anything is listed, give the user this command and wait for them to confirm it ran:

```bash
"/Applications/ConjureDSP/Uninstall ConjureAlign.command" --yes --keep-settings
```

If that file does not exist (an install from 1.3.0 or earlier), give them instead:
`sudo rm -rf /Library/Audio/Plug-Ins/{CLAP,VST3,Components}/ConjureAlign.*`, plus one
`sudo pkgutil --forget <id>` per id listed above. A leftover system copy is the usual reason
"my changes didn't show up".

**AU cache:**

```bash
killall -9 AudioComponentRegistrar 2>/dev/null; rm -rf ~/Library/Caches/AudioUnitCache
```

**Settings** (`~/Library/Application Support/ConjureDSP/ConjureAlign/`, holding the consent
answers and the install ID): keep them by default. Delete them only if the user asks for a
clean-install test, and say first that the first-run question will come back and Mixpanel and
Sentry will see a new install:

```bash
rm -rf ~/Library/Application\ Support/ConjureDSP/ConjureAlign
```

**Check nothing is left:**

```bash
find ~/Library/Audio/Plug-Ins /Library/Audio/Plug-Ins -maxdepth 2 -iname 'ConjureAlign.*' 2>/dev/null
```

## Step 2: Build from this checkout

```bash
case "$ROOT" in */.claude/worktrees/*) echo WORKTREE ;; *) echo MAIN ;; esac
```

Main checkout: `cargo xtask bundle conjure_align --release`

Worktree (plain `cargo xtask` would silently build the main checkout instead):

```bash
cargo build --release -p xtask
LINK=$(mktemp -d)/aa && ln -s "$ROOT" "$LINK" && CARGO_MANIFEST_DIR="$LINK" "$ROOT/target/release/xtask" bundle conjure_align --release
```

Use `bundle-universal` in place of `bundle` only if the user asks for an Intel + Apple silicon build.

Check the bundles came from this build:

```bash
ls -ld "$ROOT"/target/bundled/ConjureAlign.{clap,vst3,component}
```

If one is missing or older than the build you just ran, stop and tell the user.

## Step 3: Install all three, and check the AU

```bash
cp -R "$ROOT"/target/bundled/ConjureAlign.clap ~/Library/Audio/Plug-Ins/CLAP/ && \
cp -R "$ROOT"/target/bundled/ConjureAlign.vst3 ~/Library/Audio/Plug-Ins/VST3/ && \
cp -R "$ROOT"/target/bundled/ConjureAlign.component ~/Library/Audio/Plug-Ins/Components/
```

Run auval with a scratch `HOME`, because it loads the plug-in in this process and this Mac's real
settings would send real analytics:

```bash
T=$(mktemp -d) && mkdir -p "$T/Library/Application Support/ConjureDSP/ConjureAlign" &&
echo '{"consent":"declined","updates":"declined"}' > "$T/Library/Application Support/ConjureDSP/ConjureAlign/analytics.json" &&
killall -9 AudioComponentRegistrar 2>/dev/null;
HOME="$T" CONJURE_ALIGN_ANALYTICS_URL=http://127.0.0.1:9/ CONJURE_ALIGN_UPDATE_URL=http://127.0.0.1:9/ \
CONJURE_ALIGN_SENTRY_DSN=http://k@127.0.0.1:9/0 auval -v aufx ALGN CONJ 2>&1 | tail -30
```

A pass ends with `AU VALIDATION SUCCEEDED` and shows the channel line `[2, 2]  [1, 1]`. If it
shows only `[2, 2]`, the `deps/clap-wrapper-rs` patch is missing and Logic will hide the plug-in
on mono tracks. Ignore these known warnings: `MusicDeviceMIDIEvent … type 'aufx'`, missing Tail
Time, no offline rendering. auval does not test the sidechain or the editor.

Then `screen-turn release --who "<same title>"`.

## Step 4: Report

- the branch and commit installed, and whether the tree had uncommitted changes
- what was removed, including anything the user removed with their password, and whether
  settings were deleted
- the auval result, including the `[2, 2]  [1, 1]` line
- restart the DAW; in Logic, check Settings → Plug-in Manager → ConjureDSP ("Reset & Rescan
  Selection" if it is missing)

You have not opened a host, so do not say it works in one.

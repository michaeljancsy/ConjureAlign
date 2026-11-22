# Releasing ConjureAlign (macOS pkg and Windows installer)

A release build, the tag push, `gh release create` and anything that publishes or uploads happen only after Michael types yes in the session doing the work. (Moved from CLAUDE.md: Commands > Release; the step-by-step sequence is moved from the memory note conjurealign-rename-and-release.md.)

## What `scripts/release.sh` builds

- Release: `./scripts/release.sh` — universal build, sign, notarize, and package into
  `dist/ConjureAlign-<version>-macOS.pkg`, a signed installer whose component packages
  target `/Library/Audio/Plug-Ins/{VST3,CLAP,Components}` (AU postinstall clears the
  AudioComponentRegistrar cache). Signing the pkg needs the "Developer ID **Installer**"
  cert — a different cert from the "Developer ID Application" one that signs the bundles.
  `--no-notarize` builds without the multi-minute notarization step; it is checked as `$1`,
  so it must be the FIRST argument.
  - Each format package carries a **`preinstall` that sweeps its own format** out of
    `/Library` and every real user's `~/Library` before its payload lands (generated once by
    `make_scripts`, which stamps `SUBDIR`/`BUNDLE`/`CLEAR_AU_CACHE` onto one shared body,
    `packaging/macos/preinstall.in`). That body lives in a file rather than a heredoc so
    `tests/macos_packaging.rs` can stamp the same three variables onto the same template —
    `release.sh` refuses to run from a worktree and signs and notarizes, so a test could not
    otherwise drive what actually ships.
    `BundleIsRelocatable=false` stops Installer *redirecting* onto a hand-installed
    `~/Library` copy but cannot remove it, and that shadow copy is the classic "my update
    didn't take" bug. Per-format, not one global sweep package: a component's own preinstall
    is guaranteed to run before its own payload — definitional — whereas a separate sweep
    package would depend on components installing in `choices-outline` order, which Apple
    does not document and which is not something to put in front of an `rm -rf` of what you
    just installed.
  - What makes the per-format sweep *complete* is **`customize="never"`**. All three
    components always install, so all three preinstalls always run, so between them they
    cover every format — a global sweep's coverage with none of its ordering risk. Removing
    the format checkboxes is a product decision first (2026-08-31): per-format deselection
    is the only way a Mac can end up with formats at DIFFERENT versions, and "works in Logic
    but not REAPER" is then a version mismatch nobody can see. It also matches the Windows
    installer, which has no `[Components]` section and always writes both formats.
    **Reintroducing per-format choice silently breaks the sweep**, because a preinstall on a
    deselected component never runs; the `<options>` element carries that warning inline.
  - That script must never fail an install (a non-zero preinstall aborts the whole
    installation), so it runs with neither `set -e` nor `set -u` and always `exit 0`. That
    makes its two guards load-bearing rather than decorative: it refuses an empty
    `$SUBDIR`/`$BUNDLE` (which would turn the `rm -rf` into one that takes every plug-in on
    the machine) and refuses any `BUNDLE` not named `ConjureAlign.*`. **`tests/macos_packaging.rs`
    covers all of this** — it builds a plug-in tree under a scratch directory, passes it as
    `$3`, and checks that only the matching format disappears while the other two formats,
    another vendor's bundle, and (for non-AU packages) the AU cache all survive. Honouring
    `$3` is what makes that possible — and it is also required for real, since
    `enable_localSystem` still lets the user pick a non-boot volume.
    Two things that test knows and a reader should not have to rediscover. The emptiness
    guard is only *independently* observable through `$SUBDIR`: an empty `$BUNDLE` is also
    rejected by the `ConjureAlign.*` name guard below it, so that half only bites when both
    are gone — which is the point of having two. And an empty `$SUBDIR` collapses
    `"$1/$SUBDIR/$BUNDLE"` to `"$1//ConjureAlign.vst3"`, whose doubled slash resolves, so
    the guard is what stands between a mis-stamp and deleting a bundle sitting directly
    under `Plug-Ins`. Every guard in both scripts was verified by mutation (2026-09-02):
    break it, and exactly one test goes red.
  - A fourth, hidden component installs `/Applications/ConjureDSP/Uninstall
    ConjureAlign.command` (`scripts/uninstall-macos.sh`). It is `visible="false"
    start_selected="true" start_enabled="false"` — always installed, never a checkbox,
    because an uninstaller that exists only when someone ticked it is missing exactly when
    it is needed. It still needs a `<line>` in `choices-outline` (an unreferenced choice is
    inert) **and** both `<pkg-ref>`s: the inner one inside the choice and the outer one
    naming `uninstall.pkg`, or `productbuild --package-path` cannot resolve it.
  - That component deliberately does NOT go through `build_component()`. Its payload is a
    shell script, not a bundle, so `pkgbuild --analyze` emits an empty array and the
    helper's `PlistBuddy -c "Add :0:…"` fails — under `set -e` that kills the release.
    `--component-plist` is optional and configures bundle-specific behaviour only, so it is
    omitted. `--install-location` is the leaf `/Applications/ConjureDSP`, not
    `/Applications`: pkgbuild puts a `.` entry in the BOM carrying the payload root's own
    mode, and `.` maps to the install-location, so aiming at `/Applications` would have that
    entry describe `/Applications` itself. Verified: the BOM contains only `.` and the
    `.command` at `root:wheel 0755`, and `/Applications` never appears.
  - The uninstaller re-execs itself under `sudo` for one privileged phase rather than using
    `osascript … with administrator privileges`, which attributes the password dialog to
    "osascript" — indistinguishable from malware — and fails with no window server. That is
    safe only because the pkg installs the file `root:wheel 0755` inside a `root:wheel 0755`
    directory; relocating it anywhere user-writable would make it a privilege escalation.
    Its body is one `{ … }` group so the self-delete at the end cannot leave bash reading a
    truncated file. `/Applications/ConjureDSP` is removed with `rmdir`, never `rm -rf` — it
    is shared with the other ConjureDSP products — and only the `ConjureAlign` child of
    `~/Library/Application Support/ConjureDSP` is ever deleted, for the same reason.
    `tests/macos_packaging.rs` pins those three, plus the `case "$SELF" in "$INSTALL_DIR"/*)`
    guard that stops a source-checkout run deleting the checkout's own copy. Unlike the
    sweep, this script takes no destination volume — every path is an absolute constant — so
    the test drives a copy with those roots rebased onto a scratch tree, behind `PATH` stubs
    for `sudo`/`dscl`/`pkgutil`/`killall`. A rebased copy only tests something while the
    rebase still matches, so each replacement asserts it found exactly one occurrence:
    rename a constant and the test fails loudly instead of testing a script nobody ships.
  - The user-home enumeration is duplicated between the preinstall sweep and the
    uninstaller's `user_homes()`, and both carry a `KEEP IN SYNC` comment. That is now
    mechanical: the two are not textually identical (one falls through to a sweep, the other
    prints), so the test asserts the thing that must actually match — given identical `dscl`
    output, both select the same set of homes. Same idea as the receipt-id guard in
    `release.sh` and the uninstall GUID duplicated into `windows.yml`.

## Release sequence

Only after Michael's typed yes in this session for the release build and for publishing.

1. On a `claude/release-<v>` branch, bump the version in `Cargo.toml` and `Cargo.lock` only. No
   other file carries the version; the README uses `<version>` placeholders.
2. Open a PR, wait for the Windows workflow, merge.
3. In the main checkout: `git fetch origin main`, then `git merge --ff-only origin/main` (two
   separate commands).
4. Run the Sentry upload pre-flight (docs/reporting.md), then `./scripts/release.sh`. It takes about
   8 minutes; run it detached with `nohup` and follow a log, because the Bash tool's 10-minute limit
   is too tight.
5. Check the pkg before publishing: `pkgutil --expand-full` it in the scratchpad and check the
   Distribution (`customize="never"`, four `<line>`s, the hidden uninstaller choice, four
   `pkg-ref`s at the new version), the uninstall.pkg payload (0755, only the
   `com.apple.provenance` attribute; the `._` BOM entries are that attribute, not quarantine), the
   three bundles' `CFBundleShortVersionString`, and `spctl -a -t install` plus `stapler validate`.
6. Capture this release's example reports: the usage event with the most fields, a crash report
   and the unclean-shutdown report, exactly as this version sends them. The harness lives in the
   private test-results repository, in the `harness/` folder of the newest existing
   `conjurealign/*-example-reports/` folder (the previous release's; the first is
   `conjurealign/2026-11-22-v1.4.1-example-reports/`).
   - Make a scratch worktree of the merge commit, outside the main checkout, and copy that
     folder's `harness/example_reports.rs` into its `tests/` folder. Never copy it into the main
     checkout. Remove the worktree when the capture is done.
   - Run that folder's `harness/capture-all.sh <worktree> <output folder>`, with an output
     folder in a scratch location outside both repositories. It builds the harness in debug,
     does a dry run that prints the addresses the plug-in would use and stops unless all three
     are this computer, then captures into `<output folder>/captured.json`. The run must never
     reach the real services: a scratch `HOME`, all three override addresses
     (`CONJURE_ALIGN_ANALYTICS_URL`, `CONJURE_ALIGN_SENTRY_DSN`, `CONJURE_ALIGN_UPDATE_URL`) on a
     stand-in server on 127.0.0.1, and a sandbox that refuses every connection except to this
     computer.
   - Run that folder's `python3 -I harness/decode.py <output folder>/captured.json <new folder>`,
     where the new folder is `conjurealign/<date>-v<new version>-example-reports/` in the
     test-results repository. This step is what removes private details: it writes the example
     files and the raw capture with the project token, the scratch paths and this Mac's home
     folder replaced by placeholders. `captured.json` itself is never committed.
   - `decode.py` is written for one version: it hard-codes the version, the commit, the capture
     date and its file and line citations. Update it together with the harness and `notes.md`,
     and if the release changes what is sent or when, update all three before capturing and check
     the README's Privacy section.
   - Copy the updated harness into the new folder's `harness/`, and write its `notes.md`: every
     event and when it is sent, with file and line, and every way the capture differs from a
     release build.
7. Push an annotated tag `v<x.y.z>` titled `ConjureAlign <x.y.z>` on the merge commit
   (`git cat-file -t v<x.y.z>` must print `tag`). The tag push starts the Windows workflow (about
   10 minutes).
8. Soon after, `gh release create` with the notes and the pkg. The Windows job attaches
   `ConjureAlign-<v>-Windows-Setup.exe` only if the Release exists by then; otherwise it warns and
   must be re-run.
9. Tell the website PM if the release changes anything the /align page or the privacy policy says
   (features, formats, system requirements, the demo video, or what is sent). `/align/download`
   follows the latest GitHub release by itself.
10. Send the website's maintainers the example reports from step 6 and their `notes.md`, for the
    privacy page, with every release, even when nothing that is sent has changed: the page shows
    the released version's own reports, so its examples must come from that version.

Notes:
- The tag name is load-bearing: the update check reads `/releases/latest` and parses `tag_name` to
  three numbers. Drafts and pre-releases are skipped, so marking a bad release pre-release is how
  to stop advertising it.
- `release.sh` prints `line 304: visible: command not found` on every run: a comment inside the
  unquoted distribution heredoc contains backticks. Only the comment is mangled; the pkg is fine.
- Notarization uses the keychain profile "ConjureDSP-Notarize" (team A4R63LAVLS), shared with the
  other ConjureDSP products.

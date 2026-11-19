# Windows: CI build, installer and version stamp

There is no local Windows toolchain or Windows machine. Windows ships on GitHub's checks only (Michael, 2026-11-19: keep using GitHub testing). (Moved from CLAUDE.md: Commands > Windows and the build.rs bullet.)

## The Windows workflow

- Windows: no local toolchain — `.github/workflows/windows.yml` builds, tests, bundles and
  validates (pluginval `--skip-gui-tests`; clap-validator tolerating exactly the 4 known
  upstream failures) on `windows-latest`, packages `ConjureAlign-<v>-Windows-Setup.exe` with
  Inno Setup (`packaging/windows/ConjureAlign.iss`; Inno 6 is preinstalled on the runner, so
  no install step), uploads it as an artifact, and attaches it to the GitHub Release on `v*`
  tag pushes (warns if the release doesn't exist yet — create it and re-run the job). The
  editor is untestable in CI (no GPU); GUI checks need a real Windows machine.
  - The runner IS a real Windows machine and the workflow uses it as one: the installer is
    **smoke-tested end to end** (install over a planted loose-DLL mis-install → install again
    over a planted `ConjureAlign-1.2.0\` leftover and a per-user copy, with a decoy that must
    survive → uninstall), and pluginval is re-run against the *installed* bundle. That is not
    belt-and-braces: an Inno 6 installer cannot be inspected offline — 7-Zip does not read the
    format and `innoextract` lags Inno releases by years — so running it is the only
    verification that exists.
  - `AppId` in the `.iss` is a permanent identity, exactly like the AU `subtype`. Change it
    and a later installer stops recognising existing installs: two Add/Remove entries, two
    uninstallers, no upgrade path. The smoke test hard-codes the same GUID on purpose, so
    changing one without the other fails CI instead of shipping.
  - Two Inno settings are load-bearing and silently no-op if wrong.
    `CloseApplicationsFilter` defaults to `*.exe,*.dll,*.chm`, which matches neither `.vst3`
    nor `.clap` — without the override, `CloseApplications=yes` checks nothing and an install
    against a running DAW half-fails. And `ignoreversion` on `[Files]` is mandatory now that
    the DLL carries a version resource, or Inno skips a file whose installed copy is
    equal-or-newer and a same-version reinstall installs nothing.
  - Passing the version: `#define AppVersion GetEnv("CONJUREALIGN_VERSION")`, **not**
    `ISCC /DAppVersion=…`. The step is `shell: bash`, i.e. Git Bash, whose MSYS layer rewrites
    a leading-slash argument into a Windows path — `/DAppVersion=1.3.0` arrives as
    `D:\AppVersion=1.3.0`. The same hazard is why the installer itself is driven from `pwsh`
    (`/VERYSILENT` would be mangled too).

## `build.rs`: the version stamp

- `build.rs` exists for exactly one thing: on a Windows host it stamps a `VS_VERSION_INFO`
  resource into the cdylib (via `winresource` → the Windows SDK's `rc.exe`), so an installed
  `ConjureAlign.vst3` / `.clap` — both of which are this DLL renamed, with no other identity
  on disk — can be asked which build it is. It is a no-op on every other target. Note the
  two version shapes: the STRING `FileVersion` is `CARGO_PKG_VERSION` verbatim (three-part,
  matching the release tag and the only shape `update::parse_version` accepts), while the
  binary `VS_FIXEDFILEINFO` block is always `MAJOR.MINOR.PATCH.0`. Explorer will NOT display
  either — its property sheet resolves per-extension and `.vst3`/`.clap` have no registered
  handler — so the check is `(Get-Item '<path>').VersionInfo.FileVersion` in PowerShell, and
  CI asserts both blocks on the bundled files after every build.

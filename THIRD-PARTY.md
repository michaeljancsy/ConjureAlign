# Third-party components

ConjureAlign is licensed GPL-3.0-or-later (see [LICENSE](LICENSE)). Its binaries link the
following third-party components, all under GPL-compatible licenses. This file satisfies the
attribution/notice requirements of the MIT, ISC and Apache-2.0 components when binaries are
distributed.

| Component | License | Notes |
|---|---|---|
| [nih-plug](https://github.com/robbert-vdh/nih-plug) (`nih_plug`, `nih_plug_egui`, `nih_plug_xtask`) | ISC | Plugin framework. © Robbert van der Helm |
| [vst3-sys](https://github.com/RustAudio/vst3-sys) (via nih-plug's `nih_export_vst3!`) | **GPL-3.0** | VST3 bindings — the component that makes GPL-3.0 mandatory for the whole plugin |
| [clap-sys](https://github.com/glowcoil/clap-sys) | MIT OR Apache-2.0 | CLAP bindings |
| [clap-wrapper (Rust crate)](https://crates.io/crates/clap-wrapper) | MIT OR Apache-2.0 | Vendored at `deps/clap-wrapper-rs` with local patches (see `deps/PATCHES.md`) |
| [free-audio/clap-wrapper](https://github.com/free-audio/clap-wrapper) (C++, vendored by the crate) | MIT | CLAP→AudioUnit v2 translation layer |
| [Apple AudioUnitSDK](https://github.com/apple/AudioUnitSDK) (vendored by clap-wrapper) | Apache-2.0 | © Apple Inc. |
| [egui](https://github.com/emilk/egui) | MIT OR Apache-2.0 | GUI toolkit |
| [egui-baseview](https://github.com/BillyDM/egui-baseview) | MIT | egui↔baseview glue |
| [baseview](https://github.com/RustAudio/baseview) | MIT OR Apache-2.0 | Windowing; built from a patched fork (see `deps/PATCHES.md`) |
| [rustfft](https://github.com/ejmahler/RustFFT) / [realfft](https://github.com/HEnquist/realfft) | MIT (realfft), MIT OR Apache-2.0 (rustfft) | FFT for cross-correlation analysis |
| `atomic_refcell` | MIT OR Apache-2.0 | |
| `atomic_float` | MIT OR Apache-2.0 OR Zlib | |
| [native-tls](https://github.com/sfackler/rust-native-tls) | MIT OR Apache-2.0 | TLS for the opt-in analytics sender; binds the OS stack via `security-framework` (MIT OR Apache-2.0, macOS) and `schannel` (MIT, Windows). macOS/Windows only |
| `serde_json` | MIT OR Apache-2.0 | Analytics payload |
| `getrandom` | MIT OR Apache-2.0 | Random analytics device id |
| [sentry-rust](https://github.com/getsentry/sentry-rust) (`sentry`, `sentry-core`, `sentry-types`, `sentry-backtrace`, `sentry-contexts`, `sentry-debug-images`, `sentry-panic`) | MIT | Opt-in crash reporting. © Functional Software, Inc. macOS/Windows only |
| [ureq](https://github.com/algesten/ureq) (`ureq`, `ureq-proto`) | MIT OR Apache-2.0 | HTTP client for the crash reporter, on the same OS TLS stack as `native-tls` above. macOS/Windows only |
| `findshlibs`, `debugid`, `uuid`, `hostname`, `os_info`, `uname` | MIT OR Apache-2.0 (`debugid`: Apache-2.0; `hostname`, `os_info`: MIT) | Crash-report metadata (loaded images, build ids, OS name) |
| `rand`, `rand_chacha`, `rand_core`, `ppv-lite86`, `hex`, `httpdate`, `http`, `httparse`, `base64`, `base64ct`, `der`, `pem-rfc7468`, `rustls-pki-types`, `zeroize`, `utf8-zero` | MIT OR Apache-2.0 | Transitively required by the two above |
| [webpki-root-certs](https://github.com/rustls/webpki-roots) | CDLA-Permissive-2.0 | Pulled in unavoidably by `ureq`'s `native-tls` feature. Linked but never consulted: the agent is configured with `RootCerts::PlatformVerifier`, so TLS uses the OS trust store |

Where a component is dual-licensed MIT OR Apache-2.0, it is used under the MIT license.
The full MIT/ISC license texts require reproduction of the copyright notice, which the
links above provide; the Apache-2.0 text is available at
<https://www.apache.org/licenses/LICENSE-2.0>.

The complete dependency graph (including transitive crates, all MIT/Apache-2.0-compatible)
is recorded in `Cargo.lock`; `cargo metadata` lists each crate's license expression. Note that
`Cargo.lock` is a superset of what ships: it pins optional dependencies that no enabled feature
selects (`tokio`, `reqwest`, `hyper`, `actix-web` and friends arrive that way via `sentry` and
are never compiled). `cargo tree -e normal --target <triple>` is what actually lists the linked
set.

VST® is a trademark of Steinberg Media Technologies GmbH. This plugin's VST3 support is
built from the GPLv3-licensed `vst3-sys` bindings, not the proprietary Steinberg VST3 SDK
license.

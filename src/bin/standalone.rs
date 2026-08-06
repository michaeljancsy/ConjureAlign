//! Dev-only standalone wrapper so the GUI can be run and inspected without a
//! DAW: `cargo run --bin standalone --features standalone -- --backend dummy`
//!
//! Only works because of the baseview `[patch]` in Cargo.toml: every upstream
//! baseview rev nih-plug pins null-derefs in `becomeFirstResponder` on recent
//! macOS and aborts before the window opens. See CLAUDE.md "Known upstream
//! issues". `cargo run --example gui_preview --features gui-preview` is the
//! non-interactive alternative (renders PNGs offscreen).

fn main() {
    nih_plug::nih_export_standalone::<audio_align::AudioAlign>();
}

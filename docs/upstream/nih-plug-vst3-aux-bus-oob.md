# nih-plug upstream issue draft: VST3 `process()` reads past `ProcessData.inputs` when a host omits a trailing bus

**Status: DRAFT — not filed.** For Michael to review and file by hand. Nothing in this
repository posts it anywhere. The local mitigation we ship in the meantime is the second
patch on the vendored `deps/nih-plug` (see `deps/PATCHES.md`); the diff proposed below is that
patch with our `LOCAL PATCH (ConjureAlign)` markers left in, so strip those before pasting.

**Where to file (checked 2026-10-02).** As for the teardown draft
(`nih-plug-worker-teardown.md`): robbert-vdh/nih-plug is in maintenance mode, so
<https://github.com/robbert-vdh/nih-plug/issues/new> is for the record, and the report matters
more at the community successor, **nice-plug** (<https://codeberg.org/RustAudio/nice-plug>) —
check that its `wrapper/vst3/wrapper.rs` still carries the `aux_input_idx > data.num_outputs`
guard before filing there (not checked on 2026-10-02: the file could not be fetched
anonymously).

Suggested title:

> VST3: `process()` indexes `data.inputs` past `numInputs` when the host omits a trailing
> deactivated aux bus (wrong count, off-by-one), reading and dereferencing memory behind the
> host's array

Everything below the rule is the proposed issue body.

---

## Summary

In `src/wrapper/vst3/wrapper.rs`, the loop in `process()` that picks up auxiliary *input*
busses from the host's `ProcessData` is guarded by

```rust
if aux_input_idx > data.num_outputs as usize { break; }
let audio_input = &*data.inputs.add(aux_input_idx);
```

That compares the input bus index against the **output** bus count, and with `>` where `>=`
is needed. The aux *output* loop below it has the same `>` off-by-one (on the right count),
and the CLAP wrapper mirrors both (`>` on `audio_inputs_count` / `audio_outputs_count`).

For a plugin with a main input plus one sidechain input (the common sidechain-compressor
shape: `aux_input_idx == 1`) and a single output bus (`num_outputs == 1`), a host that passes
`numInputs == 1` — main bus only — passes the guard and the wrapper reads `data.inputs[1]`:
one `AudioBusBuffers` past the end of the host's array. Whatever `buffers` pointer sits
there is then dereferenced as channel data in `buffer_management.rs` (`from_raw_parts_mut` +
`copy_from_slice`). If that memory happens to be zero the plugin gets silence; otherwise it is
an access violation inside the plugin's DLL on the first process call.

## Why a host does that

Steinberg's VST 3 FAQ on processing
(<https://steinbergmedia.github.io/vst3_dev_portal/pages/FAQ/Processing.html>), verbatim:

> When these deactivated busses are the last busses (for input or output), the host do not
> need to provide associated AudioBusBuffers, in this case the plug-in should check numInputs
> and numOutputs and doesn't process these busses.

An unrouted sidechain at insert time is exactly a trailing deactivated input bus. A host that
follows that sentence crashes every nih-plug plugin with a sidechain the moment it is
inserted; a host that always passes all busses, with null `channelBuffers32` for inactive
ones, never trips it — which is why the bug survives routine testing in REAPER, Ableton Live,
Bitwig and Studio One. We found it chasing a crash-on-insert report from UAD LUNA on Windows
that reproduces on no other host we have (consistent with the mechanism; not confirmed by a
dump).

## The relevant code

All links pin master as of
[`f36931f`](https://github.com/robbert-vdh/nih-plug/blob/f36931f7af4646065488a9845d8f8c2f95252c23/src/wrapper/vst3/wrapper.rs);
the lines are unchanged on current master as of 2026-10-02.

- VST3 aux-input guard: [`wrapper.rs#L1262-L1284`](https://github.com/robbert-vdh/nih-plug/blob/f36931f7af4646065488a9845d8f8c2f95252c23/src/wrapper/vst3/wrapper.rs#L1262-L1284)
- VST3 aux-output guard: [`wrapper.rs#L1286-L1308`](https://github.com/robbert-vdh/nih-plug/blob/f36931f7af4646065488a9845d8f8c2f95252c23/src/wrapper/vst3/wrapper.rs#L1286-L1308)
- CLAP mirrors: [`clap/wrapper.rs#L2076-L2098`](https://github.com/robbert-vdh/nih-plug/blob/f36931f7af4646065488a9845d8f8c2f95252c23/src/wrapper/clap/wrapper.rs#L2076-L2098) and the output loop right after it
- What then happens to an aux-input `ChannelPointers`: [`buffer_management.rs#L260-L300`](https://github.com/robbert-vdh/nih-plug/blob/f36931f7af4646065488a9845d8f8c2f95252c23/src/wrapper/util/buffer_management.rs#L260-L300)

Fixing the guards alone is not enough, because nothing downstream expected a `None` aux bus
either: the aux-input arm of `buffer_management.rs` has a `nih_debug_assert!(is_some())`
directly above the zero-fill that handles `None`; the aux-output `None` arm asserts
`output_slices.len() == 0` (never true — that is the declared channel count) and then empties
the channel slices while keeping `num_samples`, producing a `Buffer` whose `samples()` says N
over channels of length 0, which `Buffer::set_slices` documents as a panic waiting to happen;
and the "every output slice must be non-empty" validity scan in both wrappers' `process()`
then vetoes the whole block for an omitted aux *output*, so the plugin silently stops
processing. The diff below treats an omitted input bus as silence of the block length and an
omitted output bus as a zero-length buffer that the validity scan skips.

## Reproduction

A unit test against `Wrapper<P>` directly, no host involved (appendix). It builds a plugin
with a stereo main pair, one stereo sidechain input and one stereo aux output, drives
`initialize` → `setup_processing` → `set_active` → `set_processing` → `process` with
`numInputs == numOutputs == 1`, and places a fully formed poison `AudioBusBuffers` right
behind each one-element array — exactly where the old guards read. Against master:

```
cargo test -p nih_plug --lib vst3::wrapper::tests
```

fails because the plugin's sidechain input contains the poison value (12345.0) instead of
silence; with only the output guard reverted it fails because the plugin wrote into the
poison output bus. With the diff below it passes.

## Suggested fix

Behavior-preserving for every host that passes all busses; happy to turn this plus the test
into a PR if the direction is welcome.

```diff
diff --git a/src/wrapper/clap/wrapper.rs b/src/wrapper/clap/wrapper.rs
index 3b6a63e..b7aa8b8 100644
--- a/src/wrapper/clap/wrapper.rs
+++ b/src/wrapper/clap/wrapper.rs
@@ -2080,7 +2080,10 @@ impl<P: ClapPlugin> Wrapper<P> {
                                 .enumerate()
                             {
                                 let aux_input_idx = aux_input_no + aux_input_start_idx;
-                                if aux_input_idx > process.audio_inputs_count as usize {
+                                // LOCAL PATCH (ConjureAlign): `>` let `aux_input_idx == audio_inputs_count`
+                                // through, one port past the end. CLAP hosts must pass every port, so this is
+                                // defensive; the VST3 wrapper carries the live version. See deps/PATCHES.md.
+                                if aux_input_idx >= process.audio_inputs_count as usize {
                                     break;
                                 }
 
@@ -2104,7 +2107,8 @@ impl<P: ClapPlugin> Wrapper<P> {
                                 .enumerate()
                             {
                                 let aux_output_idx = aux_output_no + aux_output_start_idx;
-                                if aux_output_idx > process.audio_outputs_count as usize {
+                                // LOCAL PATCH (ConjureAlign): same off-by-one as the input loop above.
+                                if aux_output_idx >= process.audio_outputs_count as usize {
                                     break;
                                 }
 
@@ -2133,6 +2137,10 @@ impl<P: ClapPlugin> Wrapper<P> {
                     buffers
                         .aux_outputs
                         .iter()
+                        // LOCAL PATCH (ConjureAlign): an aux output bus the host omitted is a
+                        // zero-length buffer (see `buffer_management.rs`), not a host that failed
+                        // to provide buffers, so it must not veto processing the block.
+                        .filter(|buffer| buffer.samples() > 0)
                         .flat_map(|buffer| buffer.as_slice_immutable().iter()),
                 ) {
                     if output_buffer_slice.is_empty() {
diff --git a/src/wrapper/util/buffer_management.rs b/src/wrapper/util/buffer_management.rs
index f8b0089..a9ad94d 100644
--- a/src/wrapper/util/buffer_management.rs
+++ b/src/wrapper/util/buffer_management.rs
@@ -265,8 +265,14 @@ impl BufferManager {
             )
         {
             // Since these buffers are backed by our own storage, we can fill them with zeroes if
-            // the pointers are missing for whatever reason that might be
-            nih_debug_assert!(input_channel_pointers.is_some());
+            // the pointers are missing for whatever reason that might be.
+            //
+            // LOCAL PATCH (ConjureAlign): the `nih_debug_assert!(is_some())` that stood here is
+            // gone. A missing pointer set is no longer a wrapper bug: the VST3 wrapper now
+            // leaves it `None` when the host omits a trailing deactivated input bus, which the
+            // VST3 spec permits, and the zero-fill below is exactly the right outcome for it.
+            // Under `cfg(test)` that assert is a hard `debug_assert!`, which is also what
+            // would have failed the regression test in `wrapper/vst3/wrapper.rs`.
             match input_channel_pointers {
                 Some(input_channel_pointers) => {
                     nih_debug_assert_eq!(input_channel_pointers.num_channels, input_storage.len());
@@ -323,42 +329,42 @@ impl BufferManager {
             .iter()
             .zip(self.aux_output_buffers.iter_mut())
         {
+            // LOCAL PATCH (ConjureAlign): a bus the host did not hand over becomes a
+            // ZERO-length buffer. Upstream kept `num_samples` and emptied the channel slices
+            // inside the same `set_slices` call, which breaks the `Buffer` invariant that every
+            // channel is `num_samples` long (its per-sample iterators index into those
+            // slices), and asserted `output_slices.len() == 0` first — a condition that can
+            // never hold, since the slice count is the port's declared channel count. Both
+            // fired on every block in which a host omitted a trailing aux output bus, which
+            // the VST3 spec allows, and the assert hard-fails under `cfg(test)`.
+            let Some(output_channel_pointers) = output_channel_pointers else {
+                output_buffer.set_slices(0, |output_slices| output_slices.fill_with(|| &mut []));
+                continue;
+            };
+
             output_buffer.set_slices(num_samples, |output_slices| {
-                match output_channel_pointers {
-                    Some(output_channel_pointers) => {
-                        nih_debug_assert_eq!(
-                            output_slices.len(),
-                            output_channel_pointers.num_channels
-                        );
-                        for (channel_idx, output_slice) in output_slices
-                            .iter_mut()
-                            .enumerate()
-                            .take(output_channel_pointers.num_channels)
-                        {
-                            let output_channel_pointer =
-                                output_channel_pointers.ptrs.as_ptr().add(channel_idx);
-
-                            *output_slice = std::slice::from_raw_parts_mut(
-                                (*output_channel_pointer).add(sample_offset),
-                                num_samples,
-                            );
-
-                            // The host may not zero out the buffers, and assume the plugin always
-                            // write something there
-                            output_slice.fill(0.0);
-                        }
-
-                        // If the caller/host should have provided buffer pointers but didn't then
-                        // we must get rid of any dangling slices
-                        output_slices[output_channel_pointers.num_channels..].fill_with(|| &mut [])
-                    }
-                    None => {
-                        nih_debug_assert_eq!(output_slices.len(), 0);
+                nih_debug_assert_eq!(output_slices.len(), output_channel_pointers.num_channels);
+                for (channel_idx, output_slice) in output_slices
+                    .iter_mut()
+                    .enumerate()
+                    .take(output_channel_pointers.num_channels)
+                {
+                    let output_channel_pointer =
+                        output_channel_pointers.ptrs.as_ptr().add(channel_idx);
 
-                        // Same as above
-                        output_slices.fill_with(|| &mut [])
-                    }
+                    *output_slice = std::slice::from_raw_parts_mut(
+                        (*output_channel_pointer).add(sample_offset),
+                        num_samples,
+                    );
+
+                    // The host may not zero out the buffers, and assume the plugin always
+                    // write something there
+                    output_slice.fill(0.0);
                 }
+
+                // If the caller/host should have provided buffer pointers but didn't then
+                // we must get rid of any dangling slices
+                output_slices[output_channel_pointers.num_channels..].fill_with(|| &mut [])
             });
         }
 
diff --git a/src/wrapper/vst3/wrapper.rs b/src/wrapper/vst3/wrapper.rs
index f284af5..bffd32d 100644
--- a/src/wrapper/vst3/wrapper.rs
+++ b/src/wrapper/vst3/wrapper.rs
@@ -1266,7 +1266,12 @@ impl<P: Vst3Plugin> IAudioProcessor for Wrapper<P> {
                                     .enumerate()
                                 {
                                     let aux_input_idx = aux_input_no + aux_input_start_idx;
-                                    if aux_input_idx > data.num_outputs as usize {
+                                    // LOCAL PATCH (ConjureAlign): upstream compared against `num_outputs`, with `>`.
+                                    // A host that omits a trailing deactivated input bus (the VST3 spec allows it:
+                                    // "the plug-in should check numInputs and numOutputs") therefore made this
+                                    // read one `AudioBusBuffers` PAST the host's array and dereference whatever
+                                    // pointer it found there. See deps/PATCHES.md.
+                                    if aux_input_idx >= data.num_inputs.max(0) as usize {
                                         break;
                                     }
 
@@ -1290,7 +1295,8 @@ impl<P: Vst3Plugin> IAudioProcessor for Wrapper<P> {
                                     .enumerate()
                                 {
                                     let aux_output_idx = aux_output_no + aux_output_start_idx;
-                                    if aux_output_idx > data.num_outputs as usize {
+                                    // LOCAL PATCH (ConjureAlign): same off-by-one as the input loop above.
+                                    if aux_output_idx >= data.num_outputs.max(0) as usize {
                                         break;
                                     }
 
@@ -1318,6 +1324,10 @@ impl<P: Vst3Plugin> IAudioProcessor for Wrapper<P> {
                             buffers
                                 .aux_outputs
                                 .iter()
+                                // LOCAL PATCH (ConjureAlign): an aux output bus the host omitted is a
+                                // zero-length buffer (see `buffer_management.rs`), not a host that failed
+                                // to provide buffers, so it must not veto processing the block.
+                                .filter(|buffer| buffer.samples() > 0)
                                 .flat_map(|buffer| buffer.as_slice_immutable().iter()),
                         )
                     {
```

## A second route to the same read: a deactivated bus with null channel pointers

*(Added 2026-11-09, after the guard fix above shipped locally and a JUCE-based test host still
crashed at the same line.)*

A host does not have to omit the bus. JUCE's VST3 hosting hands a deactivated bus over with
its channel count intact, every `channelBuffers32` entry null and every `silenceFlags` bit set.
nih-plug ignores `activateBus`, so `process()` takes that for a live sidechain, and
`BufferManager::create_buffers` copies from address zero:

```rust
channel.copy_from_slice(std::slice::from_raw_parts_mut(
    (*input_channel_pointer).add(sample_offset),   // null
    num_samples,
))
```

The main input's out-of-place copy just above it has the same shape. Suggested addition: in
both loops, read a null channel pointer as silence (`fill(0.0)` and move on) instead of
following it. Both the guard fix and this one are needed; either alone leaves one way in.

The aux input's zero-fill arms also never resize, so an omitted bus (or one with fewer channels
than the port) keeps the storage's previous length, `max_buffer_size` at first. A block
shorter than the maximum then gets a sidechain longer than the block. Resizing to
`num_samples` there stays within the reserved capacity and does not allocate.

Reproduced on macOS with Plug-in Bench (a JUCE-based plug-in tester), whose "Bus layouts" check
crashed every run until both changes were in. Regression tests: see the appendix's module,
which now also has `a_deactivated_aux_bus_with_null_channel_pointers_is_silence`,
`a_main_input_with_null_channel_pointers_is_silence` and `an_omitted_aux_bus_is_one_block_long`.

## Environment

- nih-plug master @ `f36931f7af4646065488a9845d8f8c2f95252c23`; the four guards and the
  `buffer_management.rs` arms are unchanged on master as of 2026-10-02
- Verified on macOS 26 (Darwin 25.3), rustc 1.98; the bug is platform-independent
- Found via a VST3/CLAP/AU plugin with a sidechain input, reported crashing on insert in
  UAD LUNA 2.0.5 on Windows. Reproduced on macOS with Plug-in Bench (2026-11-09), through the
  null-pointer route above

## Appendix: the regression test

<details>
<summary>Diff adding the test module to <code>src/wrapper/vst3/wrapper.rs</code></summary>

```diff
diff --git a/src/wrapper/vst3/wrapper.rs b/src/wrapper/vst3/wrapper.rs
index f284af5..bffd32d 100644
--- a/src/wrapper/vst3/wrapper.rs
+++ b/src/wrapper/vst3/wrapper.rs
@@ -1893,3 +1903,202 @@ impl<P: Vst3Plugin> IUnitInfo for Wrapper<P> {
         kInvalidArgument
     }
 }
+
+// LOCAL PATCH (ConjureAlign): regression test for the aux-bus bounds fix in `process()` above.
+// Run with
+//
+//     cargo test --manifest-path Cargo.toml -p nih_plug --lib vst3::wrapper::tests
+//
+// Against the unpatched `>`/`num_outputs` guards it fails on both the sidechain input and the
+// aux output assertion. See deps/PATCHES.md in the plugin repository.
+#[cfg(test)]
+mod tests {
+    use super::*;
+
+    use std::sync::atomic::{AtomicU32, AtomicUsize};
+    use vst3_sys::vst::{AudioBusBuffers, ProcessData, ProcessSetup, SymbolicSampleSizes};
+
+    use crate::prelude::*;
+
+    /// The first sample of the sidechain input's channel 0 as the plug-in saw it in its last
+    /// `process()` call, as `f32` bits. `u32::MAX` means `process()` never ran.
+    static AUX_IN_FIRST_SAMPLE_BITS: AtomicU32 = AtomicU32::new(u32::MAX);
+    /// `Buffer::samples()` of that sidechain input.
+    static AUX_IN_SAMPLES: AtomicUsize = AtomicUsize::new(usize::MAX);
+
+    /// What sits in the bus descriptors the host did NOT hand over. If the wrapper reads past
+    /// `numInputs`/`numOutputs`, these are what it finds.
+    const POISON_IN: f32 = 12345.0;
+    const POISON_OUT: f32 = 777.0;
+    /// What the plug-in writes into whatever aux output buffer it is given.
+    const MARKER: f32 = -1.0;
+
+    struct NoParams;
+
+    unsafe impl Params for NoParams {
+        fn param_map(&self) -> Vec<(String, ParamPtr, String)> {
+            Vec::new()
+        }
+    }
+
+    /// Stereo in/out plus one stereo sidechain input and one stereo aux output — the same
+    /// bus shape as a plug-in with a reference input, plus an aux output so the mirror
+    /// guard is exercised too.
+    struct SidechainPlugin {
+        params: Arc<NoParams>,
+    }
+
+    impl Default for SidechainPlugin {
+        fn default() -> Self {
+            Self {
+                params: Arc::new(NoParams),
+            }
+        }
+    }
+
+    impl Plugin for SidechainPlugin {
+        const NAME: &'static str = "aux-bus bounds test";
+        const VENDOR: &'static str = "ConjureAlign";
+        const URL: &'static str = "";
+        const EMAIL: &'static str = "";
+        const VERSION: &'static str = "0.0.0";
+
+        const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[AudioIOLayout {
+            main_input_channels: NonZeroU32::new(2),
+            main_output_channels: NonZeroU32::new(2),
+            aux_input_ports: &[new_nonzero_u32(2)],
+            aux_output_ports: &[new_nonzero_u32(2)],
+            ..AudioIOLayout::const_default()
+        }];
+
+        type SysExMessage = ();
+        type BackgroundTask = ();
+
+        fn params(&self) -> Arc<dyn Params> {
+            self.params.clone()
+        }
+
+        fn process(
+            &mut self,
+            _buffer: &mut Buffer,
+            aux: &mut AuxiliaryBuffers,
+            _context: &mut impl ProcessContext<Self>,
+        ) -> ProcessStatus {
+            let aux_in = &aux.inputs[0];
+            AUX_IN_SAMPLES.store(aux_in.samples(), Ordering::SeqCst);
+            let first = aux_in
+                .as_slice_immutable()
+                .first()
+                .and_then(|channel| channel.first().copied())
+                .unwrap_or(f32::NAN);
+            AUX_IN_FIRST_SAMPLE_BITS.store(first.to_bits(), Ordering::SeqCst);
+
+            for channel in aux.outputs[0].as_slice() {
+                channel.fill(MARKER);
+            }
+
+            ProcessStatus::Normal
+        }
+    }
+
+    impl Vst3Plugin for SidechainPlugin {
+        const VST3_CLASS_ID: [u8; 16] = *b"CnjrAlignAuxBus!";
+        const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] = &[Vst3SubCategory::Fx];
+    }
+
+    fn channel_pointers(channels: &mut [Vec<f32>]) -> Vec<*mut c_void> {
+        channels
+            .iter_mut()
+            .map(|channel| channel.as_mut_ptr() as *mut c_void)
+            .collect()
+    }
+
+    /// The VST3 spec lets a host leave out trailing deactivated busses: "the plug-in should
+    /// check numInputs and numOutputs and doesn't process these busses". So the host here
+    /// passes `numInputs == numOutputs == 1` — main busses only — while the memory right
+    /// behind each one-element array holds a fully formed poison bus, exactly where the old
+    /// guard (`idx > num_outputs`, and `num_outputs` for the *input* loop) read. The plug-in
+    /// must see silence on its sidechain and must not be handed the poison output.
+    #[test]
+    fn omitted_trailing_aux_busses_are_silence_not_the_memory_behind_the_array() {
+        const N: usize = 16;
+
+        let mut main_in = vec![vec![0.25f32; N]; 2];
+        let mut poison_in = vec![vec![POISON_IN; N]; 2];
+        let mut main_out = vec![vec![0.0f32; N]; 2];
+        let mut poison_out = vec![vec![POISON_OUT; N]; 2];
+        let mut main_in_ptrs = channel_pointers(&mut main_in);
+        let mut poison_in_ptrs = channel_pointers(&mut poison_in);
+        let mut main_out_ptrs = channel_pointers(&mut main_out);
+        let mut poison_out_ptrs = channel_pointers(&mut poison_out);
+
+        let mut inputs = [
+            AudioBusBuffers {
+                num_channels: 2,
+                silence_flags: 0,
+                buffers: main_in_ptrs.as_mut_ptr(),
+            },
+            AudioBusBuffers {
+                num_channels: 2,
+                silence_flags: 0,
+                buffers: poison_in_ptrs.as_mut_ptr(),
+            },
+        ];
+        let mut outputs = [
+            AudioBusBuffers {
+                num_channels: 2,
+                silence_flags: 0,
+                buffers: main_out_ptrs.as_mut_ptr(),
+            },
+            AudioBusBuffers {
+                num_channels: 2,
+                silence_flags: 0,
+                buffers: poison_out_ptrs.as_mut_ptr(),
+            },
+        ];
+
+        let wrapper = Wrapper::<SidechainPlugin>::new();
+        unsafe {
+            assert_eq!(wrapper.initialize(std::ptr::null_mut()), kResultOk);
+            let setup = ProcessSetup {
+                process_mode: ProcessModes::kRealtime as i32,
+                symbolic_sample_size: SymbolicSampleSizes::kSample32 as i32,
+                max_samples_per_block: N as i32,
+                sample_rate: 48_000.0,
+            };
+            assert_eq!(wrapper.setup_processing(&setup), kResultOk);
+            assert_eq!(wrapper.set_active(1), kResultOk);
+            assert_eq!(wrapper.set_processing(1), kResultOk);
+
+            // All-zero is a valid `ProcessData`: null event/parameter queues and context.
+            let mut data: ProcessData = MaybeUninit::zeroed().assume_init();
+            data.process_mode = ProcessModes::kRealtime as i32;
+            data.symbolic_sample_size = SymbolicSampleSizes::kSample32 as i32;
+            data.num_samples = N as i32;
+            data.num_inputs = 1;
+            data.num_outputs = 1;
+            data.inputs = inputs.as_mut_ptr();
+            data.outputs = outputs.as_mut_ptr();
+            assert_eq!(wrapper.process(&mut data), kResultOk);
+
+            assert_eq!(wrapper.set_processing(0), kResultOk);
+            assert_eq!(wrapper.set_active(0), kResultOk);
+        }
+
+        // The main path ran: the wrapper copied the main input through to the main output.
+        assert!(main_out.iter().flatten().all(|&s| s == 0.25));
+        assert_eq!(AUX_IN_SAMPLES.load(Ordering::SeqCst), N);
+        // The omitted sidechain arrived as silence — not as the bus behind the host's array.
+        assert_eq!(
+            f32::from_bits(AUX_IN_FIRST_SAMPLE_BITS.load(Ordering::SeqCst)),
+            0.0,
+            "the plug-in's sidechain input was read from past the end of the host's inputs"
+        );
+        assert!(poison_in.iter().flatten().all(|&s| s == POISON_IN));
+        // And the omitted aux output was not written through to that memory either.
+        assert!(
+            poison_out.iter().flatten().all(|&s| s == POISON_OUT),
+            "the plug-in's aux output was aimed past the end of the host's outputs"
+        );
+    }
+}
```

</details>

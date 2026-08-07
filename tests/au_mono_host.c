// Minimal AU host: instantiate AudioAlign, force a MONO stream format on the main busses,
// initialise, and render. Verifies that clap-wrapper's audio-ports-config selection really
// reached the CLAP plugin — if it did not, the wrapped plugin still believes it is stereo
// while the buffers are mono, and processes a shape it did not agree to.
//
// Asserts the negotiated bus formats after Initialize: every bus, including the sidechain
// ("Reference"), must report the requested width. The sidechain collapsing to 1 channel in
// the mono run is the proof that select() propagated, so it is a hard failure (exit 1), not
// just something printed — the recipe below chains the two runs on their exit codes.
//
// Not a cargo test: it needs the .component installed in ~/Library/Audio/Plug-Ins/Components/.
//   clang -O1 -framework AudioToolbox -framework CoreFoundation -o /tmp/au_mono_host tests/au_mono_host.c
//   /tmp/au_mono_host 1 && /tmp/au_mono_host 2

#include <AudioToolbox/AudioToolbox.h>
#include <stdio.h>
#include <stdlib.h>
#include <math.h>

static UInt32 g_channels = 1;

static OSStatus inputCallback(void *inRefCon, AudioUnitRenderActionFlags *ioActionFlags,
                              const AudioTimeStamp *inTimeStamp, UInt32 inBusNumber,
                              UInt32 inNumberFrames, AudioBufferList *ioData) {
  (void)inRefCon; (void)ioActionFlags; (void)inTimeStamp; (void)inBusNumber;
  // Deterministic non-silent material so the plugin's capture path has something to chew on.
  for (UInt32 b = 0; b < ioData->mNumberBuffers; ++b) {
    float *d = (float *)ioData->mBuffers[b].mData;
    for (UInt32 i = 0; i < inNumberFrames; ++i)
      d[i] = 0.25f * sinf((float)(i % 128) * 0.05f);
  }
  return noErr;
}

// Prints the negotiated format and returns its channel count, or 0 if it could not be read.
static UInt32 describe(AudioUnit au, AudioUnitScope scope, UInt32 elem, const char *label) {
  AudioStreamBasicDescription asbd;
  UInt32 sz = sizeof(asbd);
  OSStatus e = AudioUnitGetProperty(au, kAudioUnitProperty_StreamFormat, scope, elem, &asbd, &sz);
  if (e != noErr) { printf("  %-22s <error %d>\n", label, (int)e); return 0; }
  printf("  %-22s %u ch @ %.0f Hz\n", label, (unsigned)asbd.mChannelsPerFrame, asbd.mSampleRate);
  return asbd.mChannelsPerFrame;
}

// The load-bearing check: a bus that did not follow the requested width means select() never
// reached the CLAP plugin, so the AU is feeding it buffers of a shape it does not expect.
static int expect_channels(UInt32 got, UInt32 want, const char *label) {
  if (got == want) return 1;
  printf("FAIL: %s reports %u ch, expected %u — audio-ports-config select() did not reach "
         "the plugin\n", label, (unsigned)got, (unsigned)want);
  return 0;
}

static int setFormat(AudioUnit au, AudioUnitScope scope, UInt32 elem, UInt32 ch, double sr) {
  AudioStreamBasicDescription asbd = {0};
  asbd.mSampleRate = sr;
  asbd.mFormatID = kAudioFormatLinearPCM;
  asbd.mFormatFlags = kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked |
                      kAudioFormatFlagIsNonInterleaved;
  asbd.mChannelsPerFrame = ch;
  asbd.mBitsPerChannel = 32;
  asbd.mFramesPerPacket = 1;
  asbd.mBytesPerFrame = 4;
  asbd.mBytesPerPacket = 4;
  OSStatus e = AudioUnitSetProperty(au, kAudioUnitProperty_StreamFormat, scope, elem,
                                    &asbd, sizeof(asbd));
  if (e != noErr) {
    printf("  FAILED to set %s elem %u to %u ch: %d\n",
           scope == kAudioUnitScope_Input ? "input" : "output", (unsigned)elem, (unsigned)ch, (int)e);
    return 0;
  }
  return 1;
}

int main(int argc, char **argv) {
  if (argc > 1) g_channels = (UInt32)atoi(argv[1]);
  const double sr = 48000.0;
  const UInt32 frames = 512;

  AudioComponentDescription desc = {0};
  desc.componentType = 'aufx';
  desc.componentSubType = 'ALGN';
  desc.componentManufacturer = 'CONJ';

  AudioComponent comp = AudioComponentFindNext(NULL, &desc);
  if (!comp) { printf("FAIL: component aufx/ALGN/CONJ not found\n"); return 1; }

  AudioUnit au;
  if (AudioComponentInstanceNew(comp, &au) != noErr) { printf("FAIL: instantiate\n"); return 1; }

  printf("Requesting %u-channel main busses\n", (unsigned)g_channels);
  int ok = 1;
  ok &= setFormat(au, kAudioUnitScope_Input, 0, g_channels, sr);
  ok &= setFormat(au, kAudioUnitScope_Output, 0, g_channels, sr);
  if (!ok) { printf("FAIL: host could not negotiate %u ch\n", (unsigned)g_channels); return 1; }

  UInt32 maxFrames = frames;
  AudioUnitSetProperty(au, kAudioUnitProperty_MaximumFramesPerSlice, kAudioUnitScope_Global, 0,
                       &maxFrames, sizeof(maxFrames));

  // Feed both the main bus and the sidechain, so the reference path actually carries data
  // instead of falling back to the wrapper's substitute silent buffer.
  AURenderCallbackStruct cb = { inputCallback, NULL };
  if (AudioUnitSetProperty(au, kAudioUnitProperty_SetRenderCallback, kAudioUnitScope_Input, 0,
                           &cb, sizeof(cb)) != noErr) {
    printf("FAIL: could not attach input callback\n"); return 1;
  }
  if (AudioUnitSetProperty(au, kAudioUnitProperty_SetRenderCallback, kAudioUnitScope_Input, 1,
                           &cb, sizeof(cb)) != noErr) {
    printf("FAIL: could not attach Reference (bus 1) callback\n"); return 1;
  }

  OSStatus e = AudioUnitInitialize(au);
  if (e != noErr) { printf("FAIL: AudioUnitInitialize -> %d\n", (int)e); return 1; }

  printf("Negotiated after Initialize:\n");
  UInt32 in0 = describe(au, kAudioUnitScope_Input, 0, "input bus 0 (main)");
  UInt32 in1 = describe(au, kAudioUnitScope_Input, 1, "input bus 1 (Reference)");
  UInt32 out0 = describe(au, kAudioUnitScope_Output, 0, "output bus 0");

  ok &= expect_channels(in0, g_channels, "input bus 0 (main)");
  ok &= expect_channels(in1, g_channels, "input bus 1 (Reference)");
  ok &= expect_channels(out0, g_channels, "output bus 0");
  if (!ok) return 1;

  Float64 latency = 0; UInt32 lsz = sizeof(latency);
  AudioUnitGetProperty(au, kAudioUnitProperty_Latency, kAudioUnitScope_Global, 0, &latency, &lsz);
  printf("  %-22s %.6f s (%.0f samples)\n", "reported latency", latency, latency * sr);

  // Render several blocks; a layout mismatch tends to trip the sanitizer/guard pages or
  // produce NaNs rather than fail cleanly, so check the output too.
  AudioBufferList *abl = (AudioBufferList *)calloc(
      1, sizeof(AudioBufferList) + sizeof(AudioBuffer) * (g_channels ? g_channels - 1 : 0));
  abl->mNumberBuffers = g_channels;
  for (UInt32 b = 0; b < g_channels; ++b) {
    abl->mBuffers[b].mNumberChannels = 1;
    abl->mBuffers[b].mDataByteSize = frames * 4;
    abl->mBuffers[b].mData = calloc(frames, 4);
  }

  AudioTimeStamp ts = {0};
  ts.mFlags = kAudioTimeStampSampleTimeValid;
  int bad = 0;
  for (int blk = 0; blk < 32; ++blk) {
    AudioUnitRenderActionFlags flags = 0;
    ts.mSampleTime = (Float64)(blk * (int)frames);
    e = AudioUnitRender(au, &flags, &ts, 0, frames, abl);
    if (e != noErr) { printf("FAIL: render block %d -> %d\n", blk, (int)e); return 1; }
    for (UInt32 b = 0; b < g_channels; ++b) {
      float *d = (float *)abl->mBuffers[b].mData;
      for (UInt32 i = 0; i < frames; ++i)
        if (!isfinite(d[i])) { bad = 1; break; }
    }
  }
  if (bad) { printf("FAIL: non-finite samples in output\n"); return 1; }

  AudioUnitUninitialize(au);
  AudioComponentInstanceDispose(au);
  printf("PASS: %u-channel instantiate + initialize + 32 render blocks clean\n",
         (unsigned)g_channels);
  return 0;
}

// Opens ConjureAlign's real Audio Unit window in this process and checks what a host relies on while
// it is NOT running the plug-in through audio, which is how Logic treats a stopped track in a new
// project. Adapted from ConjureWidth's tools/au_host_test.m: same checks, ConjureAlign's controls
// and window layout. No audio device is used and nothing is written outside the folders below.
//
// The build is loaded from COMPONENT_PATH for this process only, so nothing has to be installed. The
// window sits behind every other window and ignores real mouse clicks; events go straight to its view.
//
// The plug-in keeps its consent answers under $HOME and, with consent given, reports a load to
// analytics, starts a crash-reporting session and checks for updates. So run it with HOME pointing at
// a scratch folder whose settings decline both, and the three addresses pointing nowhere:
//
//   clang -fobjc-arc -Wno-deprecated-declarations -framework Cocoa -framework AudioToolbox -framework AudioUnit \
//     tests/au_host_test.m -o target/au_host_test
//   T=$(mktemp -d) && mkdir -p "$T/Library/Application Support/ConjureDSP/ConjureAlign" &&
//   echo '{"consent":"declined","updates":"declined"}' > "$T/Library/Application Support/ConjureDSP/ConjureAlign/analytics.json" &&
//   HOME="$T" CONJURE_ALIGN_ANALYTICS_URL=http://127.0.0.1:9/ CONJURE_ALIGN_UPDATE_URL=http://127.0.0.1:9/ \
//   CONJURE_ALIGN_SENTRY_DSN=http://k@127.0.0.1:9/0 \
//   COMPONENT_PATH=target/bundled/ConjureAlign.component target/au_host_test "$T"
//
// The test refuses to run if HOME is the real home folder. Exit code 0 means every check passed.
#import <AudioToolbox/AudioToolbox.h>
#import <AudioUnit/AUCocoaUIView.h>
#import <Cocoa/Cocoa.h>
#include <pthread.h>
#include <pwd.h>
#include <stdatomic.h>

static AudioUnit u;
static atomic_int thread_alive = 1, render_on = 0, in_render = 0;
static atomic_long renders = 0;
static int failures = 0;
static const char *shots;

// The host's names for the controls.
static const char *TRIM = "Manual Trim", *ALIGN = "Alignment On", *POLARITY = "Polarity";

static void check(const char *what, int ok, const char *fmt, double a, double b) {
    char detail[160]; snprintf(detail, sizeof detail, fmt, a, b);
    printf("%s  %s (%s)\n", ok ? "pass" : "FAIL", what, detail);
    if (!ok) failures++;
}

static OSStatus input_cb(void *r, AudioUnitRenderActionFlags *f, const AudioTimeStamp *t, UInt32 b, UInt32 n, AudioBufferList *io) {
    for (UInt32 c = 0; c < io->mNumberBuffers; c++) memset(io->mBuffers[c].mData, 0, n * sizeof(float));
    return noErr;
}

// Blocks are 512 samples, but the buffers are longer and the rest is filled with a marker, so a
// write outside the block shows up instead of quietly spoiling memory.
#define SPARE 8192
static float out_l[SPARE], out_r[SPARE];
static atomic_int overruns = 0;
static void render_block(double *pos) {
    for (int i = 512; i < SPARE; i++) out_l[i] = out_r[i] = 12345.f;
    AudioUnitRenderActionFlags fl = 0;
    AudioTimeStamp ts; memset(&ts, 0, sizeof ts); ts.mSampleTime = *pos; ts.mFlags = kAudioTimeStampSampleTimeValid;
    struct { UInt32 nb; AudioBuffer b[2]; } abl;
    abl.nb = 2;
    abl.b[0] = (AudioBuffer){1, 512 * sizeof(float), out_l};
    abl.b[1] = (AudioBuffer){1, 512 * sizeof(float), out_r};
    if (AudioUnitRender(u, &fl, &ts, 0, 512, (AudioBufferList *)&abl) == noErr) atomic_fetch_add(&renders, 1);
    for (int i = 512; i < SPARE; i++) if (out_l[i] != 12345.f || out_r[i] != 12345.f) { atomic_fetch_add(&overruns, 1); break; }
    *pos += 512;
}

// A change the audio thread makes just before a block, at a position inside that block, the way a
// host sends sample-accurate automation. nih-plug applies the first change of a block at its start
// and splits the block where each later one lands, so `audio_thread_pair` puts an Align change at
// the start first.
static AudioUnitParameterID trim_id, align_id; static _Atomic float audio_thread_trim = -1;
static atomic_uint audio_thread_offset = 256; static atomic_int audio_thread_pair = 0;
static void *render_loop(void *x) {
    double pos = 0;
    while (atomic_load(&thread_alive)) {
        if (atomic_load(&render_on)) { atomic_store(&in_render, 1);
            float v = atomic_exchange(&audio_thread_trim, -1);
            if (v >= 0 && atomic_load(&audio_thread_pair)) AudioUnitSetParameter(u, align_id, kAudioUnitScope_Global, 0, 1, 0);
            if (v >= 0) AudioUnitSetParameter(u, trim_id, kAudioUnitScope_Global, 0, v, atomic_load(&audio_thread_offset));
            render_block(&pos); atomic_store(&in_render, 0); usleep(10600); }
        else usleep(1000);
    }
    return NULL;
}

static void pump(double seconds) { [[NSRunLoop currentRunLoop] runUntilDate:[NSDate dateWithTimeIntervalSinceNow:seconds]]; }
static void start_audio(void) { atomic_store(&render_on, 1); pump(0.3); }
// Long enough for the wrapper to decide the host has stopped running the plug-in.
static void stop_audio(void) { atomic_store(&render_on, 0); while (atomic_load(&in_render)) usleep(100); pump(0.5); }

static AudioUnitParameterID pid(const char *name) {
    UInt32 sz = 0; AudioUnitGetPropertyInfo(u, kAudioUnitProperty_ParameterList, kAudioUnitScope_Global, 0, &sz, NULL);
    AudioUnitParameterID ids[64]; AudioUnitGetProperty(u, kAudioUnitProperty_ParameterList, kAudioUnitScope_Global, 0, ids, &sz);
    for (UInt32 i = 0; i < sz / sizeof(AudioUnitParameterID); i++) {
        AudioUnitParameterInfo in; UInt32 s = sizeof in;
        AudioUnitGetProperty(u, kAudioUnitProperty_ParameterInfo, kAudioUnitScope_Global, ids[i], &in, &s);
        if (in.cfNameString && [(__bridge NSString *)in.cfNameString isEqualToString:@(name)]) return ids[i];
    }
    printf("no parameter called %s\n", name); exit(3);
}
// What the host has been told.
// Logic runs the plug-in in a helper process and sends changes on a background thread, with or
// without audio running.
static void host_set_bg(const char *n, float v) {
    AudioUnitParameterID id = pid(n);
    dispatch_semaphore_t done = dispatch_semaphore_create(0);
    dispatch_async(dispatch_get_global_queue(QOS_CLASS_USER_INTERACTIVE, 0), ^{
        AudioUnitSetParameter(u, id, kAudioUnitScope_Global, 0, v, 0);
        dispatch_semaphore_signal(done);
    });
    dispatch_semaphore_wait(done, DISPATCH_TIME_FOREVER);
}
static AudioUnitParameterInfo param_info(const char *n) { AudioUnitParameterInfo in; memset(&in, 0, sizeof in); UInt32 s = sizeof in; AudioUnitGetProperty(u, kAudioUnitProperty_ParameterInfo, kAudioUnitScope_Global, pid(n), &in, &s); return in; }
static NSArray *value_names(const char *n) { CFArrayRef a = NULL; UInt32 s = sizeof a; if (AudioUnitGetProperty(u, kAudioUnitProperty_ParameterValueStrings, kAudioUnitScope_Global, pid(n), &a, &s) || !a) return nil; return CFBridgingRelease(a); }
static float host_val(const char *n) { AudioUnitParameterValue v = -99; AudioUnitGetParameter(u, pid(n), kAudioUnitScope_Global, 0, &v); return v; }
static void host_set(const char *n, float v) { AudioUnitSetParameter(u, pid(n), kAudioUnitScope_Global, 0, v, 0); }
// What the plug-in itself holds, read from the state it would save. Works with no audio running.
static NSData *state_json(void) {
    CFPropertyListRef pl = NULL; UInt32 s = sizeof pl;
    if (AudioUnitGetProperty(u, kAudioUnitProperty_ClassInfo, kAudioUnitScope_Global, 0, &pl, &s) != noErr || !pl) return nil;
    return CFBridgingRelease(pl)[@"data"];
}
static double plug_val(const char *key) {
    NSData *d = state_json(); if (!d) return -99;
    char pat[64]; snprintf(pat, sizeof pat, "\"%s\":", key);
    const char *hit = memmem(d.bytes, d.length, pat, strlen(pat)); if (!hit) return -98;
    hit = strchr(hit + strlen(pat), ':'); if (!hit) return -97;
    hit++;
    if (!strncmp(hit, "true", 4)) return 1;
    if (!strncmp(hit, "false", 5)) return 0;
    return atof(hit);
}
// The plug-in saves Trim in milliseconds (-10 to +10); the host sees it from 0 to 1.
static double plug_trim(void) { double ms = plug_val("trim"); return ms < -90 ? ms : (ms + 10) / 20; }

// Parameter messages a host would hear.
static int n_begin, n_value, n_end;
static void listener(void *r, void *o, const AudioUnitEvent *e, UInt64 t, AudioUnitParameterValue v) {
    if (e->mEventType == kAudioUnitEvent_BeginParameterChangeGesture) n_begin++;
    if (e->mEventType == kAudioUnitEvent_EndParameterChangeGesture) n_end++;
    if (e->mEventType == kAudioUnitEvent_ParameterValueChange) n_value++;
}

static NSWindow *win; static NSView *view, *target; static CGFloat W, H;
static NSEvent *ev(NSEventType t, CGFloat x, CGFloat y) {
    return [NSEvent mouseEventWithType:t location:NSMakePoint(x, H - y) modifierFlags:0 timestamp:[NSProcessInfo processInfo].systemUptime windowNumber:win.windowNumber context:nil eventNumber:0 clickCount:1 pressure:1];
}
static void move_to(CGFloat x, CGFloat y) { [target mouseMoved:ev(NSEventTypeMouseMoved, x, y)]; pump(0.1); }
static void press(CGFloat x, CGFloat y) { move_to(x, y); [target mouseDown:ev(NSEventTypeLeftMouseDown, x, y)]; pump(0.1); }
static void release_at(CGFloat x, CGFloat y) { [target mouseUp:ev(NSEventTypeLeftMouseUp, x, y)]; pump(0.2); }
static void click(CGFloat x, CGFloat y) { press(x, y); release_at(x, y); }
// Both clicks reach the window before it draws again, so the gap can't exceed a double-click.
static void double_click(CGFloat x, CGFloat y) {
    for (int i = 0; i < 2; i++) { [target mouseDown:ev(NSEventTypeLeftMouseDown, x, y)]; [target mouseUp:ev(NSEventTypeLeftMouseUp, x, y)]; }
    pump(0.2);
}
static void drag_steps(CGFloat x0, CGFloat y0, CGFloat x1, CGFloat y1) {
    for (int i = 1; i <= 8; i++) { [target mouseDragged:ev(NSEventTypeLeftMouseDragged, x0 + (x1 - x0) * i / 8, y0 + (y1 - y0) * i / 8)]; pump(0.05); }
}
static void drag(CGFloat x0, CGFloat y0, CGFloat x1, CGFloat y1) { press(x0, y0); drag_steps(x0, y0, x1, y1); release_at(x1, y1); }
static void shot(const char *name) {
    char cmd[1200]; snprintf(cmd, sizeof cmd, "/usr/sbin/screencapture -x -o -l%ld '%s/%s.png'", (long)win.windowNumber, shots, name);
    system(cmd);
}

static Class view_class; static id<AUCocoaUIBase> factory; static id follower; static int following;
static void open_view(void) {
    view = [factory uiViewForAudioUnit:u withSize:NSMakeSize(820, 560)];
    if (follower) [[NSNotificationCenter defaultCenter] removeObserver:follower];
    follower = [[NSNotificationCenter defaultCenter] addObserverForName:NSViewFrameDidChangeNotification object:view queue:nil usingBlock:^(NSNotification *n) {
        if (following) return;
        following = 1; W = view.frame.size.width; H = view.frame.size.height; [win setContentSize:view.frame.size]; view.frameOrigin = NSZeroPoint; following = 0;
    }];
    W = view.frame.size.width; H = view.frame.size.height;
    [win setContentSize:view.frame.size];
    [win.contentView addSubview:view];
    view.frame = NSMakeRect(0, 0, W, H);
    [win orderFrontRegardless];
    pump(0.8);
    target = view; while (target.subviews.count) target = target.subviews[0];
}
static void close_view(void) { [view removeFromSuperview]; view = nil; target = nil; pump(0.4); }

// Where things are in the window, measured from its top left corner. The controls sit in a panel
// along the bottom edge; the Align / Polarity row is centred, the Trim slider starts at the left.
static CGFloat align_x(void) { return W / 2 - 101; }
static CGFloat normal_x(void) { return W / 2 + 45.5; }
static CGFloat auto_x(void) { return W / 2 - 4.5; }
static CGFloat row_y(void) { return H - 42.5; }
static const CGFloat TRIM_LEFT = 43.5, TRIM_RIGHT = 143.5;
static CGFloat trim_x(double v) { return TRIM_LEFT + v * (TRIM_RIGHT - TRIM_LEFT); }
static CGFloat trim_y(void) { return H - 20; }
static int near(double a, double b, double tol) { return fabs(a - b) <= tol; }

int main(int ac, char **av) { @autoreleasepool {
    alarm(120); setvbuf(stdout, NULL, _IONBF, 0);
    shots = ac > 1 ? av[1] : "/tmp";
    struct passwd *pw = getpwuid(getuid());
    const char *home = getenv("HOME");
    if (!home || (pw && !strcmp(home, pw->pw_dir))) { puts("point HOME at a scratch folder first; see the top of this file"); return 2; }
    [NSApplication sharedApplication]; [NSApp setActivationPolicy:NSApplicationActivationPolicyAccessory];
    AudioComponentDescription d = { kAudioUnitType_Effect, 'ALGN', 'CONJ', 0, 0 };
    const char *path = getenv("COMPONENT_PATH");
    if (!path) { puts("set COMPONENT_PATH to the built .component"); return 2; }
    CFURLRef url = CFURLCreateFromFileSystemRepresentation(NULL, (const UInt8 *)path, strlen(path), true);
    CFBundleRef bundle = CFBundleCreate(NULL, url);
    if (!bundle || !CFBundleLoadExecutable(bundle)) { puts("could not load the build"); return 2; }
    AudioComponentFactoryFunction fn = (AudioComponentFactoryFunction)CFBundleGetFunctionPointerForName(bundle, CFSTR("GetPluginFactoryAUV2"));
    AudioComponent c = AudioComponentRegister(&d, CFSTR("ConjureDSP: ConjureAlign"), 0x00000100, fn);
    if (!c) { puts("plug-in not found"); return 2; }
    if (AudioComponentInstanceNew(c, &u)) return 2;
    AudioStreamBasicDescription f; memset(&f, 0, sizeof f);
    f.mSampleRate = 48000; f.mFormatID = kAudioFormatLinearPCM; f.mFormatFlags = kAudioFormatFlagsNativeFloatPacked | kAudioFormatFlagIsNonInterleaved;
    f.mBytesPerPacket = f.mBytesPerFrame = 4; f.mFramesPerPacket = 1; f.mChannelsPerFrame = 2; f.mBitsPerChannel = 32;
    AudioUnitSetProperty(u, kAudioUnitProperty_StreamFormat, kAudioUnitScope_Input, 0, &f, sizeof f);
    AudioUnitSetProperty(u, kAudioUnitProperty_StreamFormat, kAudioUnitScope_Output, 0, &f, sizeof f);
    UInt32 mx = 512; AudioUnitSetProperty(u, kAudioUnitProperty_MaximumFramesPerSlice, kAudioUnitScope_Global, 0, &mx, sizeof mx);
    // Only the main input is fed. The Reference input stays unconnected and reads as silence.
    AURenderCallbackStruct cb = { input_cb, NULL }; AudioUnitSetProperty(u, kAudioUnitProperty_SetRenderCallback, kAudioUnitScope_Input, 0, &cb, sizeof cb);
    host_set(TRIM, 0.1);
    if (AudioUnitInitialize(u)) return 2;

    AUEventListenerRef lis = NULL;
    AUEventListenerCreate(listener, NULL, CFRunLoopGetMain(), kCFRunLoopDefaultMode, 0.005, 0.005, &lis);
    const char *watched[] = { TRIM, ALIGN, POLARITY };
    for (int i = 0; i < 3; i++) {
        AudioUnitEvent e; memset(&e, 0, sizeof e);
        e.mArgument.mParameter = (AudioUnitParameter){ u, pid(watched[i]), kAudioUnitScope_Global, 0 };
        UInt32 types[] = { kAudioUnitEvent_ParameterValueChange, kAudioUnitEvent_BeginParameterChangeGesture, kAudioUnitEvent_EndParameterChangeGesture };
        for (int t = 0; t < 3; t++) { e.mEventType = types[t]; AUEventListenerAddEventType(lis, NULL, &e); }
    }

    UInt32 sz = 0; Boolean w; AudioUnitGetPropertyInfo(u, kAudioUnitProperty_CocoaUI, kAudioUnitScope_Global, 0, &sz, &w);
    AudioUnitCocoaViewInfo *info = malloc(sz); AudioUnitGetProperty(u, kAudioUnitProperty_CocoaUI, kAudioUnitScope_Global, 0, info, &sz);
    NSBundle *b = [NSBundle bundleWithURL:(__bridge NSURL *)info->mCocoaAUViewBundleLocation];
    view_class = [b classNamed:(__bridge NSString *)info->mCocoaAUViewClass[0]]; factory = [[view_class alloc] init];
    win = [[NSWindow alloc] initWithContentRect:NSMakeRect(60, 60, 820, 560) styleMask:NSWindowStyleMaskTitled backing:NSBackingStoreBuffered defer:NO];
    win.title = @"ConjureAlign host test";
    win.level = kCGDesktopWindowLevel; win.ignoresMouseEvents = YES; win.hidesOnDeactivate = NO;
    open_view();
    trim_id = pid(TRIM); align_id = pid(ALIGN);
    pthread_t th; pthread_create(&th, NULL, render_loop, NULL);
    shot("0_opened");

    puts("--- track stopped, no audio has ever run");
    check("a change made before setup reaches the plug-in", near(plug_trim(), 0.1, 0.002), "plug-in holds %.3f, wanted %.3f", plug_trim(), 0.1);
    host_set(TRIM, 0.5); pump(0.1);
    n_begin = n_value = n_end = 0;
    click(align_x(), row_y()); pump(0.1);
    check("clicking Align reaches the plug-in", near(plug_val("align_on"), 0, 0.01), "plug-in holds %.0f, wanted %.0f", plug_val("align_on"), 0);
    check("clicking Align reaches the host", near(host_val(ALIGN), 0, 0.01), "host holds %.0f, wanted %.0f", host_val(ALIGN), 0);
    check("the host hears a whole gesture", n_begin >= 1 && n_end == n_begin && n_value >= 1, "%.0f starts, %.0f ends", n_begin, n_end);
    click(align_x(), row_y()); pump(0.1);
    check("a second click turns Align back on", near(plug_val("align_on"), 1, 0.01) && near(host_val(ALIGN), 1, 0.01), "plug-in %.0f, host %.0f", plug_val("align_on"), host_val(ALIGN));
    click(normal_x(), row_y()); pump(0.1);
    check("clicking Polarity: Normal reaches the plug-in and the host", near(plug_val("polarity"), 1, 0.01) && near(host_val(POLARITY), 1, 0.01), "plug-in %.0f, host %.0f", plug_val("polarity"), host_val(POLARITY));
    click(auto_x(), row_y()); pump(0.1);
    check("clicking Polarity: Auto goes back", near(plug_val("polarity"), 0, 0.01) && near(host_val(POLARITY), 0, 0.01), "plug-in %.0f, host %.0f", plug_val("polarity"), host_val(POLARITY));

    n_begin = n_value = n_end = 0;
    drag(trim_x(0.02), trim_y(), trim_x(0.6), trim_y()); pump(0.1);
    double dragged = plug_trim();
    // Trim's default (0.5) is not where the drag ends, so an unchanged value can't pass.
    check("dragging Trim reaches the plug-in", near(dragged, 0.6, 0.03), "plug-in holds %.3f, wanted %.3f", dragged, 0.6);
    check("dragging Trim reaches the host", near(host_val(TRIM), dragged, 0.002), "host %.3f, plug-in %.3f", host_val(TRIM), dragged);
    check("the host hears a whole drag", n_begin >= 1 && n_end == n_begin && n_value >= 2, "%.0f starts, %.0f ends", n_begin, n_end);
    shot("1_stopped_after_drag");

    // A host's Undo goes back to the value it knew when the gesture opened, so that has to happen
    // before the press changes anything.
    host_set(TRIM, 0.1); pump(0.1);
    n_begin = n_value = n_end = 0;
    press(trim_x(0.4), trim_y()); pump(0.1);
    check("pressing a slider opens the gesture and moves the value", n_begin == 1 && near(plug_trim(), 0.4, 0.02), "%.0f starts, plug-in holds %.3f", n_begin, plug_trim());
    check("the gesture stays open while the button is held", n_end == 0, "%.0f ends, wanted %.0f", n_end, 0);
    release_at(trim_x(0.4), trim_y()); pump(0.1);
    check("releasing closes the gesture once", n_begin == 1 && n_end == 1, "%.0f starts, %.0f ends", n_begin, n_end);
    n_begin = n_value = n_end = 0;
    // egui counts a click within 0.6 s of the one before last as a triple click, so wait first.
    pump(0.7); move_to(trim_x(0.7), trim_y()); double_click(trim_x(0.7), trim_y());
    check("double-clicking a slider resets it", near(host_val(TRIM), param_info(TRIM).defaultValue, 0.002), "host holds %.3f, wanted %.3f", host_val(TRIM), param_info(TRIM).defaultValue);
    check("double-clicking leaves no gesture open", n_begin >= 1 && n_end == n_begin, "%.0f starts, %.0f ends", n_begin, n_end);

    // A host's own controls step by the unit: with none, an arrow on an on/off control moved it by
    // 0.0001.
    check("Alignment On is an on/off control", param_info(ALIGN).unit == kAudioUnitParameterUnit_Boolean, "unit %.0f, wanted %.0f", param_info(ALIGN).unit, kAudioUnitParameterUnit_Boolean);
    check("Capture is an on/off control", param_info("Capture").unit == kAudioUnitParameterUnit_Boolean, "unit %.0f, wanted %.0f", param_info("Capture").unit, kAudioUnitParameterUnit_Boolean);
    check("Polarity is a list of choices", param_info(POLARITY).unit == kAudioUnitParameterUnit_Indexed, "unit %.0f, wanted %.0f", param_info(POLARITY).unit, kAudioUnitParameterUnit_Indexed);
    check("Manual Trim stays continuous", param_info(TRIM).unit == kAudioUnitParameterUnit_Generic && !value_names(TRIM), "unit %.0f, wanted %.0f", param_info(TRIM).unit, kAudioUnitParameterUnit_Generic);
    check("Gate stays continuous", param_info("Gate").unit == kAudioUnitParameterUnit_Generic && !value_names("Gate"), "unit %.0f, wanted %.0f", param_info("Gate").unit, kAudioUnitParameterUnit_Generic);
    check("Max Shift stays continuous", param_info("Max Shift").unit == kAudioUnitParameterUnit_Generic && !value_names("Max Shift"), "unit %.0f, wanted %.0f", param_info("Max Shift").unit, kAudioUnitParameterUnit_Generic);
    NSArray *polarities = value_names(POLARITY);
    check("Polarity names its choices", polarities.count == 3 && [polarities[0] isEqual:@"Auto"] && [polarities[1] isEqual:@"Normal"] && [polarities[2] isEqual:@"Inverted"], "%.0f names, wanted %.0f", polarities.count, 3);
    // Logic's Controls view shows "Off", "On (Base Channel 1)" and "On (Base Channel 16)" for
    // values 0, 1 and 2 of the second-to-last control if that control names its values. Released
    // versions listed the controls sorted by ID, which puts Alignment On there, and this keeps
    // that order, so a new control that changes it fails here first.
    UInt32 list_size = 0; AudioUnitGetPropertyInfo(u, kAudioUnitProperty_ParameterList, kAudioUnitScope_Global, 0, &list_size, NULL);
    AudioUnitParameterID order[64]; AudioUnitGetProperty(u, kAudioUnitProperty_ParameterList, kAudioUnitScope_Global, 0, order, &list_size);
    UInt32 listed = list_size / sizeof(AudioUnitParameterID); int sorted = listed == 6;
    for (UInt32 i = 1; i < listed; i++) if (order[i - 1] >= order[i]) sorted = 0;
    check("the host lists the controls in the released order", sorted, "%.0f controls, wanted %.0f, sorted by ID", listed, 6);
    check("Alignment On is the second-to-last control", listed > 2 && order[listed - 2] == pid(ALIGN), "%.0f controls, wanted %.0f", listed, 6);
    AudioUnitParameterInfo second_last; memset(&second_last, 0, sizeof second_last); UInt32 info_size = sizeof second_last;
    AudioUnitGetProperty(u, kAudioUnitProperty_ParameterInfo, kAudioUnitScope_Global, order[listed - 2], &second_last, &info_size);
    check("the second-to-last control names no values", !(second_last.flags & kAudioUnitParameterFlag_ValuesHaveStrings), "flags %.0f, unit %.0f", second_last.flags, second_last.unit);
    check("on/off controls leave their names to the host", !(param_info(ALIGN).flags & kAudioUnitParameterFlag_ValuesHaveStrings) && !(param_info("Capture").flags & kAudioUnitParameterFlag_ValuesHaveStrings), "Alignment On flags %.0f, Capture flags %.0f", param_info(ALIGN).flags, param_info("Capture").flags);
    check("other controls still name their values", (param_info(POLARITY).flags & kAudioUnitParameterFlag_ValuesHaveStrings) && (param_info(TRIM).flags & kAudioUnitParameterFlag_ValuesHaveStrings), "Polarity flags %.0f, Trim flags %.0f", param_info(POLARITY).flags, param_info(TRIM).flags);
    float polarity_was = host_val(POLARITY);
    host_set_bg(POLARITY, 2); pump(0.1);
    check("a whole step of Polarity reaches the plug-in", near(plug_val("polarity"), 2, 0.01), "plug-in holds %.0f, wanted %.0f", plug_val("polarity"), 2);
    host_set_bg(POLARITY, polarity_was); pump(0.1);

    host_set(TRIM, 0.25); pump(0.1);
    check("a change from the host reaches the plug-in", near(plug_trim(), 0.25, 0.002), "plug-in holds %.3f, wanted %.3f", plug_trim(), 0.25);
    shot("2_stopped_host_set_25");

    host_set_bg(TRIM, 0.31f); pump(0.1);
    check("a change from a background thread reaches the plug-in", near(plug_trim(), 0.31, 0.002), "plug-in holds %.3f, wanted %.3f", plug_trim(), 0.31);
    check("the host reads that change back", near(host_val(TRIM), 0.31, 0.002), "host holds %.3f, wanted %.3f", host_val(TRIM), 0.31);
    shot("2b_stopped_background_set_31");
    host_set_bg(ALIGN, 0); pump(0.1);
    check("Align follows a background thread", near(plug_val("align_on"), 0, 0.01), "plug-in holds %.0f, wanted %.0f", plug_val("align_on"), 0);
    host_set_bg(ALIGN, 1); pump(0.1);
    check("Align follows a background thread back on", near(plug_val("align_on"), 1, 0.01), "plug-in holds %.0f, wanted %.0f", plug_val("align_on"), 1);
    host_set_bg(TRIM, 0.11f); host_set_bg(TRIM, 0.21f); pump(0.1);
    check("the latest of two quick background changes wins", near(plug_trim(), 0.21, 0.002), "plug-in holds %.3f, wanted %.3f", plug_trim(), 0.21);
    close_view();
    host_set_bg(TRIM, 0.37f); pump(0.1);
    check("a background change works with the window closed", near(plug_trim(), 0.37, 0.002), "plug-in holds %.3f, wanted %.3f", plug_trim(), 0.37);
    open_view();
    host_set_bg(TRIM, 0.25f); pump(0.1);

    drag(trim_x(0.25), trim_y(), trim_x(0.75), trim_y()); pump(0.1);
    double edited = plug_trim();
    check("the window can move Trim after the host did", edited > 0.5, "plug-in holds %.3f, %.0f", edited, 0);
    long before = atomic_load(&renders);
    start_audio();
    check("audio really started", atomic_load(&renders) > before, "%.0f blocks, %.0f", (double)(atomic_load(&renders) - before), 0);
    check("starting audio does not bring back an older value", near(plug_trim(), edited, 0.002) && near(host_val(TRIM), edited, 0.002), "plug-in %.3f, host %.3f", plug_trim(), host_val(TRIM));

    puts("--- audio running");
    n_begin = n_value = n_end = 0;
    click(align_x(), row_y()); pump(0.1);
    check("clicking Align works with audio running", near(plug_val("align_on"), 0, 0.01) && near(host_val(ALIGN), 0, 0.01), "plug-in %.0f, host %.0f", plug_val("align_on"), host_val(ALIGN));
    click(align_x(), row_y()); pump(0.1);
    drag(trim_x(0.7), trim_y(), trim_x(0.4), trim_y()); pump(0.1);
    check("dragging Trim works with audio running", near(plug_trim(), host_val(TRIM), 0.002) && plug_trim() < 0.6, "plug-in %.3f, host %.3f", plug_trim(), host_val(TRIM));
    check("the host hears whole gestures with audio running", n_begin >= 3 && n_end == n_begin, "%.0f starts, %.0f ends", n_begin, n_end);
    host_set(TRIM, 0.33); pump(0.1);
    check("a change from the host works with audio running", near(plug_trim(), 0.33, 0.002), "plug-in holds %.3f, wanted %.3f", plug_trim(), 0.33);

    host_set_bg(TRIM, 0.35f); pump(0.1);
    check("a background change works with audio running", near(plug_trim(), 0.35, 0.002), "plug-in holds %.3f, wanted %.3f", plug_trim(), 0.35);

    atomic_store(&audio_thread_trim, 0.44f); pump(0.2);
    check("a change from the audio thread, part-way into a block, works", near(plug_trim(), 0.44, 0.002), "plug-in holds %.3f, wanted %.3f", plug_trim(), 0.44);

    puts("--- moving between stopped and running");
    atomic_store(&render_on, 0); while (atomic_load(&in_render)) usleep(100);
    host_set_bg(TRIM, 0.52f); pump(0.4);
    check("a background change just after audio stops arrives", near(plug_trim(), 0.52, 0.002), "plug-in holds %.3f, wanted %.3f", plug_trim(), 0.52);
    start_audio();
    check("audio resuming does not bring back an older value", near(plug_trim(), 0.52, 0.002), "plug-in holds %.3f, wanted %.3f", plug_trim(), 0.52);
    stop_audio();
    host_set_bg(TRIM, 0.57f); atomic_store(&render_on, 1); pump(0.3);
    check("a background change just before audio starts arrives", near(plug_trim(), 0.57, 0.002), "plug-in holds %.3f, wanted %.3f", plug_trim(), 0.57);
    stop_audio();
    host_set_bg(TRIM, 0.3f); pump(0.1);
    drag(trim_x(0.3), trim_y(), trim_x(0.7), trim_y()); pump(0.1);
    double newer = plug_trim();
    start_audio();
    check("a newer window change outlasts a background change", newer > 0.5 && near(plug_trim(), newer, 0.002) && near(host_val(TRIM), newer, 0.002), "plug-in %.3f, window set %.3f", plug_trim(), newer);
    n_begin = n_value = n_end = 0;
    press(trim_x(0.33), trim_y()); drag_steps(trim_x(0.33), trim_y(), trim_x(0.4), trim_y());
    stop_audio();
    drag_steps(trim_x(0.4), trim_y(), trim_x(0.5), trim_y());
    release_at(trim_x(0.5), trim_y()); pump(0.3);
    check("a drag that starts running and ends stopped closes", n_begin == 1 && n_end == 1, "%.0f starts, %.0f ends", n_begin, n_end);
    check("that drag's value arrives", near(plug_trim(), host_val(TRIM), 0.002) && plug_trim() > 0.42, "plug-in %.3f, host %.3f", plug_trim(), host_val(TRIM));
    n_begin = n_value = n_end = 0;
    press(trim_x(0.5), trim_y()); drag_steps(trim_x(0.5), trim_y(), trim_x(0.55), trim_y());
    start_audio();
    drag_steps(trim_x(0.55), trim_y(), trim_x(0.6), trim_y());
    release_at(trim_x(0.6), trim_y()); pump(0.3);
    check("a drag that starts stopped and ends running closes", n_begin == 1 && n_end == 1, "%.0f starts, %.0f ends", n_begin, n_end);
    stop_audio();

    puts("--- saved settings, track stopped");
    host_set(TRIM, 0.2); pump(0.1);
    CFPropertyListRef saved = NULL; UInt32 psz = sizeof saved;
    AudioUnitGetProperty(u, kAudioUnitProperty_ClassInfo, kAudioUnitScope_Global, 0, &saved, &psz);
    host_set(TRIM, 0.8); pump(0.1);
    AudioUnitSetProperty(u, kAudioUnitProperty_ClassInfo, kAudioUnitScope_Global, 0, &saved, sizeof saved); pump(0.2);
    check("restoring saved settings reaches the plug-in", near(plug_trim(), 0.2, 0.002), "plug-in holds %.3f, wanted %.3f", plug_trim(), 0.2);
    check("restoring saved settings reaches the host", near(host_val(TRIM), 0.2, 0.002), "host holds %.3f, wanted %.3f", host_val(TRIM), 0.2);
    host_set_bg(TRIM, 0.8f);
    AudioUnitSetProperty(u, kAudioUnitProperty_ClassInfo, kAudioUnitScope_Global, 0, &saved, sizeof saved); pump(0.2);
    check("restoring beats a background change still waiting", near(plug_trim(), 0.2, 0.002), "plug-in holds %.3f, wanted %.3f", plug_trim(), 0.2);
    CFRelease(saved);
    shot("3_stopped_restored_20");

    puts("--- resizing, track stopped");
    CGFloat W0 = view.frame.size.width, H0 = view.frame.size.height;
    drag(W0 - 5, H0 - 5, W0 - 60, H0 - 40); pump(0.5);
    NSSize smaller = view.frame.size;
    check("dragging the corner in shrinks the window", near(smaller.width, W0 - 55, 8) && near(smaller.height, H0 - 35, 8), "now %.0f by %.0f", smaller.width, smaller.height);
    check("the drawing fills the new size", near(target.frame.size.width, smaller.width, 1) && near(target.frame.size.height, smaller.height, 1), "drawing is %.0f by %.0f", target.frame.size.width, target.frame.size.height);
    shot("4_smaller");
    drag(smaller.width - 5, smaller.height - 5, smaller.width + 140, smaller.height + 90); pump(0.5);
    NSSize larger = view.frame.size;
    check("dragging the corner out grows the window", near(larger.width, smaller.width + 145, 8) && near(larger.height, smaller.height + 95, 8), "now %.0f by %.0f", larger.width, larger.height);
    check("the drawing fills the larger size", near(target.frame.size.width, larger.width, 1) && near(target.frame.size.height, larger.height, 1), "drawing is %.0f by %.0f", target.frame.size.width, target.frame.size.height);
    shot("5_larger");
    drag(larger.width - 5, larger.height - 5, 100, 100); pump(0.5);
    check("the window stops shrinking at its smallest size", near(view.frame.size.width, 600, 2) && near(view.frame.size.height, 460, 2), "now %.0f by %.0f", view.frame.size.width, view.frame.size.height);
    drag(view.frame.size.width - 5, view.frame.size.height - 5, 735, 535); pump(0.5);
    NSSize kept = view.frame.size;
    click(align_x(), row_y()); pump(0.1);
    check("controls still respond after resizing", near(plug_val("align_on"), 0, 0.01), "plug-in holds %.0f, wanted %.0f", plug_val("align_on"), 0);
    click(align_x(), row_y()); pump(0.1);
    CFPropertyListRef sized = NULL; psz = sizeof sized;
    AudioUnitGetProperty(u, kAudioUnitProperty_ClassInfo, kAudioUnitScope_Global, 0, &sized, &psz);
    drag(view.frame.size.width - 5, view.frame.size.height - 5, view.frame.size.width + 50, view.frame.size.height + 30); pump(0.5);
    AudioUnitSetProperty(u, kAudioUnitProperty_ClassInfo, kAudioUnitScope_Global, 0, &sized, sizeof sized); pump(0.5);
    CFRelease(sized);
    check("restoring saved settings restores the size", near(view.frame.size.width, kept.width, 2) && near(view.frame.size.height, kept.height, 2), "now %.0f by %.0f", view.frame.size.width, view.frame.size.height);
    check("the drawing follows a restored size", near(target.frame.size.width, kept.width, 2) && near(target.frame.size.height, kept.height, 2), "drawing is %.0f by %.0f", target.frame.size.width, target.frame.size.height);
    close_view(); open_view();
    check("reopening keeps the size", near(view.frame.size.width, kept.width, 1) && near(view.frame.size.height, kept.height, 1), "reopened at %.0f by %.0f", view.frame.size.width, view.frame.size.height);
    shot("6_reopened");

    puts("--- stress: audio starting and stopping while controls move");
    srand48(7);
    double t_end = CFAbsoluteTimeGetCurrent() + 6; int moves = 0;
    while (CFAbsoluteTimeGetCurrent() < t_end) {
        atomic_store(&render_on, lrand48() % 2);
        for (int i = 0; i < 12; i++) { if (i % 2) host_set(TRIM, drand48()); else host_set_bg(TRIM, drand48()); moves++; [target mouseMoved:ev(NSEventTypeMouseMoved, trim_x(0.5), trim_y())]; pump(0.002); }
        click(align_x(), row_y());
        pump(drand48() * 0.3);
    }
    stop_audio();
    host_set(TRIM, 0.61); pump(0.1);
    check("still answering after the stress run", near(plug_trim(), 0.61, 0.002) && near(host_val(TRIM), 0.61, 0.002), "plug-in %.3f, host %.3f", plug_trim(), host_val(TRIM));
    printf("stress run: %d changes, %ld audio blocks in total\n", moves, atomic_load(&renders));

    // Last, because the plug-in would split its 512-sample block at sample 4096, past the end of
    // its buffers, which stock clap-wrapper allowed and which can bring the process down.
    puts("--- two changes in one block, the second placed past its end");
    atomic_store(&audio_thread_pair, 1); atomic_store(&audio_thread_offset, 4096); start_audio();
    atomic_store(&audio_thread_trim, 0.66f); pump(0.2);
    check("the second change still arrives", near(plug_trim(), 0.66, 0.002), "plug-in holds %.3f, wanted %.3f", plug_trim(), 0.66);
    stop_audio();
    check("the plug-in never wrote outside the block it was given", atomic_load(&overruns) == 0, "%.0f blocks overran, wanted %.0f", atomic_load(&overruns), 0);

    atomic_store(&thread_alive, 0); pthread_join(th, NULL);
    close_view(); [win orderOut:nil]; pump(0.2);
    AUListenerDispose(lis);
    AudioUnitUninitialize(u); AudioComponentInstanceDispose(u);
    printf("closed cleanly. %d failed\n", failures);
    return failures ? 1 : 0;
} }

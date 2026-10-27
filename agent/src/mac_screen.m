// The Mac's screen for `muxen-agent screen`: ScreenCaptureKit streams a
// display at the size asked for, and each new frame (BGRA) goes to a
// callback in Rust (see desktop.rs). Needs Screen Recording permission.

#import <AppKit/AppKit.h>
#import <Foundation/Foundation.h>
#import <CoreGraphics/CoreGraphics.h>
#import <CoreMedia/CoreMedia.h>
#import <CoreVideo/CoreVideo.h>
#import <ScreenCaptureKit/ScreenCaptureKit.h>
#import <ApplicationServices/ApplicationServices.h>

// The CGWindowID behind an Accessibility window (private, long-standing).
extern AXError _AXUIElementGetWindow(AXUIElementRef element, CGWindowID *window);

typedef void (*muxen_frame_fn)(void *ctx, const uint8_t *bgra, size_t width, size_t height, size_t stride);

@interface MuxenOutput : NSObject <SCStreamOutput, SCStreamDelegate>
@property(nonatomic) muxen_frame_fn fn;
@property(nonatomic) void *ctx;
@property(atomic) BOOL stopped;
@end

@implementation MuxenOutput
- (void)stream:(SCStream *)stream didOutputSampleBuffer:(CMSampleBufferRef)buffer ofType:(SCStreamOutputType)type {
    if (type != SCStreamOutputTypeScreen) {
        return;
    }
    // Only frames with something new on them (not "idle" repeats).
    CFArrayRef attachments = CMSampleBufferGetSampleAttachmentsArray(buffer, false);
    if (attachments && CFArrayGetCount(attachments) > 0) {
        NSDictionary *info = (__bridge NSDictionary *)CFArrayGetValueAtIndex(attachments, 0);
        NSNumber *status = info[SCStreamFrameInfoStatus];
        if (status && status.integerValue != SCFrameStatusComplete) {
            return;
        }
    }
    CVImageBufferRef pixels = CMSampleBufferGetImageBuffer(buffer);
    if (!pixels) {
        return;
    }
    CVPixelBufferLockBaseAddress(pixels, kCVPixelBufferLock_ReadOnly);
    self.fn(self.ctx, CVPixelBufferGetBaseAddress(pixels), CVPixelBufferGetWidth(pixels), CVPixelBufferGetHeight(pixels),
            CVPixelBufferGetBytesPerRow(pixels));
    CVPixelBufferUnlockBaseAddress(pixels, kCVPixelBufferLock_ReadOnly);
}

- (void)stream:(SCStream *)stream didStopWithError:(NSError *)error {
    self.stopped = YES;
}
@end

static SCStream *muxen_stream;
static MuxenOutput *muxen_output;
static SCStreamConfiguration *muxen_config;
// A window captured through its display (see `muxen_capture_window_start`):
// where that display sits on the desktop, to place the window on it.
static BOOL muxen_via_display;
static CGPoint muxen_display_at;

static void say(char *err, size_t len, NSString *what) {
    snprintf(err, len, "%s", what.UTF8String ? what.UTF8String : "unknown error");
}

/// A display (0: the main one): its size in pixels and in points (what input
/// uses), and where it sits on the desktop, in points.
void muxen_display(uint32_t id, size_t *pixels_w, size_t *pixels_h, double *points_w, double *points_h, double *x, double *y) {
    if (id == 0) {
        id = CGMainDisplayID();
    }
    CGDisplayModeRef mode = CGDisplayCopyDisplayMode(id);
    *pixels_w = mode ? CGDisplayModeGetPixelWidth(mode) : CGDisplayPixelsWide(id);
    *pixels_h = mode ? CGDisplayModeGetPixelHeight(mode) : CGDisplayPixelsHigh(id);
    if (mode) {
        CGDisplayModeRelease(mode);
    }
    CGRect bounds = CGDisplayBounds(id);
    *points_w = bounds.size.width;
    *points_h = bounds.size.height;
    *x = bounds.origin.x;
    *y = bounds.origin.y;
}

uint32_t muxen_display_main(void) {
    return CGMainDisplayID();
}

/// The displays in use; how many.
size_t muxen_display_list(uint32_t *ids, size_t max) {
    uint32_t n = 0;
    if (CGGetActiveDisplayList((uint32_t)max, ids, &n) != kCGErrorSuccess) {
        return 0;
    }
    return n;
}

int muxen_display_builtin(uint32_t id) {
    return CGDisplayIsBuiltin(id) ? 1 : 0;
}

/// The display's own name ("Studio Display", "DELL U2720Q") into `buf`; its
/// length, or 0 when macOS doesn't say.
size_t muxen_display_name(uint32_t id, char *buf, size_t len) {
    @autoreleasepool {
        if (@available(macOS 10.15, *)) {
            for (NSScreen *screen in NSScreen.screens) {
                NSNumber *number = screen.deviceDescription[@"NSScreenNumber"];
                if (number && number.unsignedIntValue == id) {
                    const char *name = screen.localizedName.UTF8String;
                    if (!name || !*name) {
                        return 0;
                    }
                    snprintf(buf, len, "%s", name);
                    return strlen(buf);
                }
            }
        }
        return 0;
    }
}

/// A display's modes, Retina versions included.
static CFArrayRef muxen_modes(uint32_t id) {
    const void *keys[] = { kCGDisplayShowDuplicateLowResolutionModes };
    const void *values[] = { kCFBooleanTrue };
    CFDictionaryRef options = CFDictionaryCreate(NULL, keys, values, 1, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    CFArrayRef modes = CGDisplayCopyAllDisplayModes(id, options);
    CFRelease(options);
    return modes;
}

/// The sizes a display can be set to, in points, as width/height pairs into
/// `sizes` (room for `max` of them); how many (repeats included).
size_t muxen_display_modes(uint32_t id, uint32_t *sizes, size_t max) {
    CFArrayRef modes = muxen_modes(id);
    if (!modes) {
        return 0;
    }
    size_t n = 0;
    for (CFIndex i = 0; i < CFArrayGetCount(modes) && n < max; i++) {
        CGDisplayModeRef mode = (CGDisplayModeRef)CFArrayGetValueAtIndex(modes, i);
        if (!CGDisplayModeIsUsableForDesktopGUI(mode)) {
            continue;
        }
        sizes[n * 2] = (uint32_t)CGDisplayModeGetWidth(mode);
        sizes[n * 2 + 1] = (uint32_t)CGDisplayModeGetHeight(mode);
        n++;
    }
    CFRelease(modes);
    return n;
}

/// Switches a display to `w`×`h` points: the sharpest version of that size
/// (Retina when it has one), at the highest refresh rate. 0 when done;
/// otherwise 1 and why in `err`.
int muxen_display_set_mode(uint32_t id, uint32_t w, uint32_t h, char *err, size_t errlen) {
    CFArrayRef modes = muxen_modes(id);
    if (!modes) {
        snprintf(err, errlen, "That display isn't on.");
        return 1;
    }
    CGDisplayModeRef best = NULL;
    for (CFIndex i = 0; i < CFArrayGetCount(modes); i++) {
        CGDisplayModeRef mode = (CGDisplayModeRef)CFArrayGetValueAtIndex(modes, i);
        if (!CGDisplayModeIsUsableForDesktopGUI(mode) || CGDisplayModeGetWidth(mode) != w || CGDisplayModeGetHeight(mode) != h) {
            continue;
        }
        if (!best || CGDisplayModeGetPixelWidth(mode) > CGDisplayModeGetPixelWidth(best) ||
            (CGDisplayModeGetPixelWidth(mode) == CGDisplayModeGetPixelWidth(best) && CGDisplayModeGetRefreshRate(mode) > CGDisplayModeGetRefreshRate(best))) {
            best = mode;
        }
    }
    int failed = 1;
    CGDisplayConfigRef config;
    if (!best) {
        snprintf(err, errlen, "That size isn't one this display offers.");
    } else if (CGBeginDisplayConfiguration(&config) == kCGErrorSuccess) {
        CGConfigureDisplayWithDisplayMode(config, id, best, NULL);
        // Kept like a choice in System Settings; a virtual display only for now.
        if (CGCompleteDisplayConfiguration(config, kCGConfigurePermanently) == kCGErrorSuccess) {
            failed = 0;
        } else if (CGBeginDisplayConfiguration(&config) == kCGErrorSuccess) {
            CGConfigureDisplayWithDisplayMode(config, id, best, NULL);
            failed = CGCompleteDisplayConfiguration(config, kCGConfigureForSession) != kCGErrorSuccess;
        }
        if (failed) {
            snprintf(err, errlen, "macOS didn't switch the display.");
        }
    }
    CFRelease(modes);
    return failed;
}

/// What's shareable on the screen now, or nil (and why in `err`).
static SCShareableContent *shareable(BOOL on_screen_only, int timeout, char *err, size_t errlen) {
    dispatch_semaphore_t done = dispatch_semaphore_create(0);
    __block SCShareableContent *content = nil;
    __block NSError *failure = nil;
    [SCShareableContent getShareableContentExcludingDesktopWindows:NO
                                               onScreenWindowsOnly:on_screen_only
                                                 completionHandler:^(SCShareableContent *c, NSError *e) {
                                                     content = c;
                                                     failure = e;
                                                     dispatch_semaphore_signal(done);
                                                 }];
    if (dispatch_semaphore_wait(done, dispatch_time(DISPATCH_TIME_NOW, timeout * NSEC_PER_SEC)) != 0) {
        say(err, errlen, @"the screen didn't answer");
        return nil;
    }
    if (!content) {
        say(err, errlen, failure ? failure.localizedDescription : @"no screen to show");
    }
    return content;
}

/// Streams what `filter` shows (of it, `source` when it isn't null) at
/// width×height, waiting up to `timeout` seconds for it to start. 0 when
/// it's going; otherwise 1 and why in `err`.
static int start_stream(SCContentFilter *filter, CGRect source, int fps, size_t width, size_t height, BOOL cursor, int timeout, muxen_frame_fn fn, void *ctx, char *err, size_t errlen) {
    SCStreamConfiguration *config = [SCStreamConfiguration new];
    config.width = width;
    config.height = height;
    if (!CGRectIsNull(source)) {
        config.sourceRect = source;
    }
    config.pixelFormat = kCVPixelFormatType_32BGRA;
    config.minimumFrameInterval = CMTimeMake(1, fps > 0 ? fps : 30);
    config.showsCursor = cursor;
    config.queueDepth = 3;
    muxen_config = config;
    muxen_output = [MuxenOutput new];
    muxen_output.fn = fn;
    muxen_output.ctx = ctx;
    muxen_stream = [[SCStream alloc] initWithFilter:filter configuration:config delegate:muxen_output];
    NSError *added = nil;
    dispatch_queue_t queue = dispatch_queue_create("muxen.screen", DISPATCH_QUEUE_SERIAL);
    if (![muxen_stream addStreamOutput:muxen_output type:SCStreamOutputTypeScreen sampleHandlerQueue:queue error:&added]) {
        say(err, errlen, added ? added.localizedDescription : @"couldn't capture the screen");
        return 1;
    }
    dispatch_semaphore_t done = dispatch_semaphore_create(0);
    __block NSError *start = nil;
    [muxen_stream startCaptureWithCompletionHandler:^(NSError *e) {
        start = e;
        dispatch_semaphore_signal(done);
    }];
    if (dispatch_semaphore_wait(done, dispatch_time(DISPATCH_TIME_NOW, timeout * NSEC_PER_SEC)) != 0) {
        say(err, errlen, @"the screen capture didn't start");
        [muxen_stream stopCaptureWithCompletionHandler:nil];
        muxen_stream = nil;
        return 1;
    }
    if (start) {
        say(err, errlen, start.localizedDescription);
        return 1;
    }
    return 0;
}

/// Starts streaming a display (0: the main one) at width×height. 0 when it's
/// going; otherwise 1 and why in `err`.
int muxen_capture_start(uint32_t id, int fps, size_t width, size_t height, muxen_frame_fn fn, void *ctx, char *err, size_t errlen) {
    @autoreleasepool {
        SCShareableContent *content = shareable(YES, 10, err, errlen);
        if (!content) {
            return 1;
        }
        CGDirectDisplayID want = id ? id : CGMainDisplayID();
        SCDisplay *display = nil;
        for (SCDisplay *d in content.displays) {
            if (d.displayID == want) {
                display = d;
            }
        }
        if (!display && id == 0) {
            display = content.displays.firstObject;
        }
        if (!display) {
            say(err, errlen, @"no display");
            return 1;
        }
        SCContentFilter *filter = [[SCContentFilter alloc] initWithDisplay:display excludingWindows:@[]];
        return start_stream(filter, CGRectNull, fps, width, height, YES, 10, fn, ctx, err, errlen);
    }
}

/// Starts streaming one window on its own (even while it's covered) at
/// width×height, without the pointer (the app shows its own). 0 when it's
/// going; 2 when the window is gone; otherwise 1 and why in `err`.
int muxen_capture_window_start(uint32_t window_id, int fps, size_t width, size_t height, muxen_frame_fn fn, void *ctx, char *err, size_t errlen) {
    @autoreleasepool {
        SCShareableContent *content = shareable(NO, 5, err, errlen);
        if (!content) {
            return 1;
        }
        SCWindow *window = nil;
        for (SCWindow *w in content.windows) {
            if (w.windowID == window_id) {
                window = w;
            }
        }
        if (!window) {
            return 2;
        }
        SCContentFilter *alone = [[SCContentFilter alloc] initWithDesktopIndependentWindow:window];
        if (start_stream(alone, CGRectNull, fps, width, height, NO, 6, fn, ctx, err, errlen) == 0) {
            return 0;
        }
        // ScreenCaptureKit sometimes never starts a lone window: the same
        // window through its display then, every other window left out (as
        // the desktop view captures, so still whole while covered).
        CGPoint middle = CGPointMake(CGRectGetMidX(window.frame), CGRectGetMidY(window.frame));
        SCDisplay *display = nil;
        for (SCDisplay *d in content.displays) {
            if (CGRectContainsPoint(d.frame, middle)) {
                display = d;
            }
        }
        if (!display) {
            return 1;
        }
        SCContentFilter *through = [[SCContentFilter alloc] initWithDisplay:display includingWindows:@[ window ]];
        muxen_via_display = YES;
        muxen_display_at = display.frame.origin;
        CGRect source = CGRectOffset(window.frame, -display.frame.origin.x, -display.frame.origin.y);
        return start_stream(through, source, fps, width, height, NO, 8, fn, ctx, err, errlen);
    }
}

/// Follows the window shown as it moves (x, y) and changes size (w×h points,
/// pw×ph pixels of frame).
void muxen_capture_follow(double x, double y, double w, double h, size_t pw, size_t ph) {
    if (!muxen_stream || !muxen_config) {
        return;
    }
    BOOL resized = muxen_config.width != pw || muxen_config.height != ph;
    if (!resized && !muxen_via_display) {
        return;
    }
    muxen_config.width = pw;
    muxen_config.height = ph;
    if (muxen_via_display) {
        muxen_config.sourceRect = CGRectMake(x - muxen_display_at.x, y - muxen_display_at.y, w, h);
    }
    [muxen_stream updateConfiguration:muxen_config completionHandler:nil];
}

// ---------------------------------------------------------------------------
// Apps' windows: the list, one window's place and size, and acting on one
// through Accessibility (bringing it to the front, resizing it).

/// The apps' windows on screen, front first, as JSON:
/// [{"id", "title", "app", "pid", "x", "y", "width", "height", "scale"}] (points).
size_t muxen_windows_json(char *buf, size_t len) {
    @autoreleasepool {
        CFArrayRef list = CGWindowListCopyWindowInfo(kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements, kCGNullWindowID);
        if (!list) {
            return 0;
        }
        NSMutableArray *out = [NSMutableArray array];
        for (NSDictionary *w in (__bridge NSArray *)list) {
            CGRect r;
            if ([w[(id)kCGWindowLayer] intValue] != 0 || [w[(id)kCGWindowAlpha] doubleValue] <= 0 ||
                !CGRectMakeWithDictionaryRepresentation((__bridge CFDictionaryRef)w[(id)kCGWindowBounds], &r) || r.size.width < 60 || r.size.height < 40) {
                continue;
            }
            NSString *app = w[(id)kCGWindowOwnerName] ?: @"";
            NSString *title = w[(id)kCGWindowName] ?: @"";
            NSScreen *on = NSScreen.mainScreen;
            for (NSScreen *screen in NSScreen.screens) {
                NSRect f = screen.frame;
                CGFloat top = NSMaxY(NSScreen.screens.firstObject.frame) - NSMaxY(f);
                if (CGRectContainsPoint(CGRectMake(f.origin.x, top, f.size.width, f.size.height), CGPointMake(CGRectGetMidX(r), CGRectGetMidY(r)))) {
                    on = screen;
                }
            }
            [out addObject:@{
                @"id" : w[(id)kCGWindowNumber],
                @"title" : title.length ? title : app,
                @"app" : app,
                @"pid" : w[(id)kCGWindowOwnerPID],
                @"x" : @(r.origin.x),
                @"y" : @(r.origin.y),
                @"width" : @(r.size.width),
                @"height" : @(r.size.height),
                @"scale" : @(on.backingScaleFactor),
            }];
        }
        CFRelease(list);
        NSData *json = [NSJSONSerialization dataWithJSONObject:out options:0 error:nil];
        if (json.length < len) {
            memcpy(buf, json.bytes, json.length);
            buf[json.length] = 0;
        }
        return json.length;
    }
}

/// One window: its place and size (points, top left of the main display at
/// 0, 0, as for input), its owner, and whether it's on screen. 0 when it's gone.
int muxen_window_info(uint32_t window_id, double *x, double *y, double *w, double *h, int32_t *pid, int *on_screen) {
    @autoreleasepool {
        CFArrayRef list = CGWindowListCopyWindowInfo(kCGWindowListOptionIncludingWindow, window_id);
        if (!list) {
            return 0;
        }
        NSArray *found = (__bridge NSArray *)list;
        int ok = 0;
        if (found.count > 0) {
            NSDictionary *info = found.firstObject;
            CGRect r;
            if (CGRectMakeWithDictionaryRepresentation((__bridge CFDictionaryRef)info[(id)kCGWindowBounds], &r)) {
                *x = r.origin.x;
                *y = r.origin.y;
                *w = r.size.width;
                *h = r.size.height;
                *pid = [info[(id)kCGWindowOwnerPID] intValue];
                *on_screen = [info[(id)kCGWindowIsOnscreen] boolValue] ? 1 : 0;
                ok = 1;
            }
        }
        CFRelease(list);
        return ok;
    }
}

/// A window's title, or its app's name when it has none.
size_t muxen_window_title(uint32_t window_id, char *buf, size_t len) {
    @autoreleasepool {
        CFArrayRef list = CGWindowListCopyWindowInfo(kCGWindowListOptionIncludingWindow, window_id);
        if (!list) {
            if (len) {
                buf[0] = 0;
            }
            return 0;
        }
        NSDictionary *info = ((__bridge NSArray *)list).firstObject;
        NSString *title = [info[(id)kCGWindowName] length] ? info[(id)kCGWindowName] : (info[(id)kCGWindowOwnerName] ?: @"");
        snprintf(buf, len, "%s", title.UTF8String ?: "");
        CFRelease(list);
        return strlen(buf);
    }
}

/// How many pixels a point is on the display at (x, y) (points, as for input).
double muxen_scale_at(double x, double y) {
    @autoreleasepool {
        CGFloat top = NSMaxY(NSScreen.screens.firstObject.frame);
        for (NSScreen *screen in NSScreen.screens) {
            NSRect f = screen.frame;
            if (CGRectContainsPoint(CGRectMake(f.origin.x, top - NSMaxY(f), f.size.width, f.size.height), CGPointMake(x, y))) {
                return screen.backingScaleFactor;
            }
        }
        return NSScreen.mainScreen.backingScaleFactor ?: 1;
    }
}

/// Where the pointer is (points, as for input).
void muxen_pointer(double *x, double *y) {
    CGEventRef e = CGEventCreate(NULL);
    CGPoint p = e ? CGEventGetLocation(e) : CGPointZero;
    if (e) {
        CFRelease(e);
    }
    *x = p.x;
    *y = p.y;
}

/// The Accessibility element of window `id` of app `pid` (retained), or NULL.
static AXUIElementRef ax_window(uint32_t window_id, int32_t pid) {
    AXUIElementRef app = AXUIElementCreateApplication(pid);
    CFArrayRef windows = NULL;
    AXUIElementRef found = NULL;
    if (AXUIElementCopyAttributeValue(app, kAXWindowsAttribute, (CFTypeRef *)&windows) == kAXErrorSuccess && windows) {
        for (CFIndex i = 0; i < CFArrayGetCount(windows); i++) {
            AXUIElementRef w = (AXUIElementRef)CFArrayGetValueAtIndex(windows, i);
            CGWindowID wid = 0;
            if (_AXUIElementGetWindow(w, &wid) == kAXErrorSuccess && wid == window_id) {
                found = (AXUIElementRef)CFRetain(w);
                break;
            }
        }
        CFRelease(windows);
    }
    CFRelease(app);
    return found;
}

/// Whether app `pid` is the one in front.
int muxen_app_in_front(int32_t pid) {
    return NSWorkspace.sharedWorkspace.frontmostApplication.processIdentifier == pid ? 1 : 0;
}

/// Brings window `id` of app `pid` to the front (out of the Dock when minimized).
void muxen_window_raise(uint32_t window_id, int32_t pid) {
    @autoreleasepool {
        AXUIElementRef w = ax_window(window_id, pid);
        if (w) {
            AXUIElementSetAttributeValue(w, kAXMinimizedAttribute, kCFBooleanFalse);
            AXUIElementPerformAction(w, kAXRaiseAction);
            CFRelease(w);
        }
        [[NSRunningApplication runningApplicationWithProcessIdentifier:pid] activateWithOptions:NSApplicationActivateIgnoringOtherApps];
    }
}

/// Resizes window `id` of app `pid` to w×h points.
void muxen_window_resize(uint32_t window_id, int32_t pid, double w, double h) {
    @autoreleasepool {
        AXUIElementRef win = ax_window(window_id, pid);
        if (!win) {
            return;
        }
        CGSize size = CGSizeMake(w, h);
        AXValueRef value = AXValueCreate(kAXValueCGSizeType, &size);
        AXUIElementSetAttributeValue(win, kAXSizeAttribute, value);
        CFRelease(value);
        CFRelease(win);
    }
}

/// What's at (x, y) (points) in app `pid`, near its window's top: 1 when a
/// press there drags the window (the title bar, an empty toolbar, a label, the
/// empty part of an Electron app's own title bar), 0 when it's something to
/// click (anything that can be pressed or typed in), -1 when the app doesn't say.
int muxen_drags_at(int32_t pid, double x, double y) {
    AXUIElementRef app = AXUIElementCreateApplication(pid);
    AXUIElementSetMessagingTimeout(app, 0.1);
    AXUIElementRef element = NULL;
    int answer = -1;
    if (AXUIElementCopyElementAtPosition(app, x, y, &element) == kAXErrorSuccess && element) {
        answer = 0;
        CFStringRef role = NULL;
        if (AXUIElementCopyAttributeValue(element, kAXRoleAttribute, (CFTypeRef *)&role) == kAXErrorSuccess && role) {
            NSString *r = (__bridge NSString *)role;
            NSArray *still = @[ @"AXWindow", @"AXToolbar", @"AXGroup", @"AXUnknown", @"AXStaticText", @"AXWebArea", @"AXSplitGroup", @"AXImage", @"AXLayoutArea" ];
            if ([still containsObject:r]) {
                CFArrayRef actions = NULL;
                BOOL pressable = NO;
                if (AXUIElementCopyActionNames(element, &actions) == kAXErrorSuccess && actions) {
                    pressable = CFArrayContainsValue(actions, CFRangeMake(0, CFArrayGetCount(actions)), kAXPressAction);
                    CFRelease(actions);
                }
                answer = pressable ? 0 : 1;
            }
            CFRelease(role);
        }
        CFRelease(element);
    }
    CFRelease(app);
    return answer;
}

/// The shape of the pointer: "arrow", "text", "hand", "ew", "ns",
/// "crosshair", "move" (the system's own shapes, told apart by size and hot spot).
const char *muxen_cursor_kind(void) {
    @autoreleasepool {
        NSCursor *now = NSCursor.currentSystemCursor;
        if (!now) {
            return "arrow";
        }
        // The arrow first: others drawn on it (with a badge) share its size and hot spot.
        struct { NSCursor *cursor; const char *kind; } shapes[] = {
            {NSCursor.arrowCursor, "arrow"},
            {NSCursor.IBeamCursor, "text"},
            {NSCursor.pointingHandCursor, "hand"},
            {NSCursor.resizeLeftRightCursor, "ew"},
            {NSCursor.resizeUpDownCursor, "ns"},
            {NSCursor.crosshairCursor, "crosshair"},
            {NSCursor.closedHandCursor, "move"},
        };
        for (size_t i = 0; i < sizeof shapes / sizeof shapes[0]; i++) {
            if (NSEqualPoints(now.hotSpot, shapes[i].cursor.hotSpot) && NSEqualSizes(now.image.size, shapes[i].cursor.image.size)) {
                return shapes[i].kind;
            }
        }
        return "arrow";
    }
}

/// Keeps the stream going but next to nothing: a tiny frame a second (see
/// `linger` in desktop.rs).
void muxen_capture_idle(void) {
    if (!muxen_stream || !muxen_config) {
        return;
    }
    muxen_config.width = 16;
    muxen_config.height = 16;
    muxen_config.minimumFrameInterval = CMTimeMake(1, 1);
    [muxen_stream updateConfiguration:muxen_config completionHandler:nil];
}

/// Stops the stream and waits a moment for ScreenCaptureKit to let it go.
void muxen_capture_stop(void) {
    if (!muxen_stream) {
        return;
    }
    dispatch_semaphore_t done = dispatch_semaphore_create(0);
    [muxen_stream stopCaptureWithCompletionHandler:^(NSError *e) {
        (void)e;
        dispatch_semaphore_signal(done);
    }];
    dispatch_semaphore_wait(done, dispatch_time(DISPATCH_TIME_NOW, 2 * NSEC_PER_SEC));
    muxen_stream = nil;
}

/// Whether the stream stopped on its own (the display went away, permission was taken back).
int muxen_capture_stopped(void) {
    return muxen_output && muxen_output.stopped ? 1 : 0;
}

// ---------------------------------------------------------------------------
// Input: mouse, scroll wheel and keyboard, as if from the Mac's own devices.
// Needs Accessibility permission.

/// Whether this program may capture the screen; `ask` shows the Mac's own
/// prompt (once) when it may not.
int muxen_capture_allowed(int ask) {
    if (CGPreflightScreenCaptureAccess()) {
        return 1;
    }
    return ask ? (CGRequestScreenCaptureAccess() ? 1 : 0) : 0;
}

/// Whether this program may send mouse and keyboard events (Accessibility).
int muxen_input_allowed(int ask) {
    if (CGPreflightPostEventAccess()) {
        return 1;
    }
    return ask ? (CGRequestPostEventAccess() ? 1 : 0) : 0;
}

/// A mouse event at (x, y) in points. `kind`: a CGEventType; `button`: 0 left,
/// 1 right, 2 middle; `clicks`: 1, or 2 for the second click of a double click.
void muxen_mouse(uint32_t kind, double x, double y, uint32_t button, int64_t clicks) {
    CGEventRef e = CGEventCreateMouseEvent(NULL, (CGEventType)kind, CGPointMake(x, y), (CGMouseButton)button);
    if (!e) {
        return;
    }
    if (clicks > 0) {
        CGEventSetIntegerValueField(e, kCGMouseEventClickState, clicks);
    }
    CGEventPost(kCGHIDEventTap, e);
    CFRelease(e);
}

/// Lines of scrolling: positive `dy` scrolls down (like X11's wheel), `dx` right.
void muxen_scroll(int32_t dy, int32_t dx) {
    CGEventRef e = CGEventCreateScrollWheelEvent(NULL, kCGScrollEventUnitLine, 2, -dy, -dx);
    if (e) {
        CGEventPost(kCGHIDEventTap, e);
        CFRelease(e);
    }
}

/// A key (macOS virtual keycode) down or up, with the modifier flags held.
void muxen_key(uint16_t keycode, int down, uint64_t flags) {
    CGEventRef e = CGEventCreateKeyboardEvent(NULL, keycode, down ? true : false);
    if (!e) {
        return;
    }
    CGEventSetFlags(e, (CGEventFlags)flags);
    CGEventPost(kCGHIDEventTap, e);
    CFRelease(e);
}

/// Types text (any characters, whatever the keyboard layout).
void muxen_text(const uint16_t *utf16, size_t len) {
    for (size_t i = 0; i < len; i += 20) {
        size_t n = len - i < 20 ? len - i : 20;
        for (int down = 1; down >= 0; down--) {
            CGEventRef e = CGEventCreateKeyboardEvent(NULL, 0, down ? true : false);
            if (!e) {
                return;
            }
            CGEventKeyboardSetUnicodeString(e, n, utf16 + i);
            CGEventPost(kCGHIDEventTap, e);
            CFRelease(e);
        }
    }
}

// ---------------------------------------------------------------------------
// Virtual displays (like DeskPad): CGVirtualDisplay, part of CoreGraphics but
// not in its public headers. The display exists while this process holds it.

@interface CGVirtualDisplayDescriptor : NSObject
@property(retain, nonatomic) NSString *name;
@property(nonatomic) unsigned int maxPixelsHigh;
@property(nonatomic) unsigned int maxPixelsWide;
@property(nonatomic) CGSize sizeInMillimeters;
@property(nonatomic) unsigned int serialNum;
@property(nonatomic) unsigned int productID;
@property(nonatomic) unsigned int vendorID;
@property(copy, nonatomic) void (^terminationHandler)(id, id);
- (void)setDispatchQueue:(dispatch_queue_t)queue;
@end

@interface CGVirtualDisplayMode : NSObject
- (instancetype)initWithWidth:(unsigned int)width height:(unsigned int)height refreshRate:(double)refreshRate;
@end

@interface CGVirtualDisplaySettings : NSObject
@property(nonatomic) unsigned int hiDPI;
@property(retain, nonatomic) NSArray *modes;
@end

@interface CGVirtualDisplay : NSObject
- (instancetype)initWithDescriptor:(CGVirtualDisplayDescriptor *)descriptor;
- (BOOL)applySettings:(CGVirtualDisplaySettings *)settings;
@property(readonly, nonatomic) unsigned int displayID;
@end

static CGVirtualDisplay *muxen_virtual;

/// Makes a virtual display of width×height (points = pixels) called `name`;
/// its id, or 0 and why in `err`. Keep the process running to keep it.
uint32_t muxen_vdisplay_create(uint32_t width, uint32_t height, const char *name, char *err, size_t errlen) {
    @autoreleasepool {
        Class descriptorClass = NSClassFromString(@"CGVirtualDisplayDescriptor");
        Class displayClass = NSClassFromString(@"CGVirtualDisplay");
        Class settingsClass = NSClassFromString(@"CGVirtualDisplaySettings");
        Class modeClass = NSClassFromString(@"CGVirtualDisplayMode");
        if (!descriptorClass || !displayClass || !settingsClass || !modeClass) {
            say(err, errlen, @"this version of macOS can't make virtual displays");
            return 0;
        }
        CGVirtualDisplayDescriptor *descriptor = [[descriptorClass alloc] init];
        [descriptor setDispatchQueue:dispatch_get_main_queue()];
        descriptor.name = [NSString stringWithUTF8String:name ? name : "Muxen"];
        // Room for every size it offers (like a real monitor's list).
        descriptor.maxPixelsWide = width > 3840 ? width : 3840;
        descriptor.maxPixelsHigh = height > 2160 ? height : 2160;
        // About 96 dpi.
        descriptor.sizeInMillimeters = CGSizeMake(width * 0.2646, height * 0.2646);
        descriptor.vendorID = 0x4d58;  // "MX"
        descriptor.productID = 0x0001;
        descriptor.serialNum = (unsigned int)getpid();
        descriptor.terminationHandler = ^(id a, id b) {
            exit(0);
        };
        muxen_virtual = [[displayClass alloc] initWithDescriptor:descriptor];
        if (!muxen_virtual || muxen_virtual.displayID == 0) {
            say(err, errlen, @"macOS refused the virtual display");
            return 0;
        }
        CGVirtualDisplaySettings *settings = [[settingsClass alloc] init];
        settings.hiDPI = 0;
        // The size asked for first (what it starts at), then the usual ones,
        // so System Settings › Displays can switch it like a real monitor.
        static const unsigned int sizes[][2] = {{3840, 2160}, {3200, 1800}, {2560, 1600}, {2560, 1440}, {2048, 1152}, {1920, 1200},
                                               {1920, 1080}, {1680, 1050}, {1600, 900}, {1440, 900}, {1280, 800}, {1280, 720}, {1024, 768}};
        NSMutableArray *modes = [NSMutableArray arrayWithObject:[[modeClass alloc] initWithWidth:width height:height refreshRate:60]];
        for (size_t i = 0; i < sizeof sizes / sizeof sizes[0]; i++) {
            if (sizes[i][0] != width || sizes[i][1] != height) {
                [modes addObject:[[modeClass alloc] initWithWidth:sizes[i][0] height:sizes[i][1] refreshRate:60]];
            }
        }
        settings.modes = modes;
        if (![muxen_virtual applySettings:settings]) {
            say(err, errlen, @"macOS didn't accept the display's size");
            muxen_virtual = nil;
            return 0;
        }
        return muxen_virtual.displayID;
    }
}

/// Holds the virtual display until the process is told to stop.
void muxen_vdisplay_run(void) {
    dispatch_main();
}

// ---------------------------------------------------------------------------
// The clipboard, for `muxen-agent clip` (see clipboard.rs).

/// The pasteboard's change count (it goes up with every copy), and in
/// `concealed` whether what's there is a password manager's: they mark their
/// copies with these types (nspasteboard.org, and 1Password's own).
long muxen_pasteboard(int *concealed) {
    @autoreleasepool {
        NSPasteboard *pb = [NSPasteboard generalPasteboard];
        NSArray *marks = @[ @"org.nspasteboard.ConcealedType", @"org.nspasteboard.TransientType", @"org.nspasteboard.AutoGeneratedType", @"com.agilebits.onepassword" ];
        *concealed = 0;
        for (NSString *type in pb.types) {
            if ([marks containsObject:type]) {
                *concealed = 1;
            }
        }
        return (long)pb.changeCount;
    }
}

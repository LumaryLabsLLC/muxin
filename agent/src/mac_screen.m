// The Mac's screen for `muxen-agent screen`: ScreenCaptureKit streams a
// display at the size asked for, and each new frame (BGRA) goes to a
// callback in Rust (see desktop.rs). Needs Screen Recording permission.

#import <AppKit/AppKit.h>
#import <Foundation/Foundation.h>
#import <CoreGraphics/CoreGraphics.h>
#import <CoreMedia/CoreMedia.h>
#import <CoreVideo/CoreVideo.h>
#import <ScreenCaptureKit/ScreenCaptureKit.h>

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

/// Starts streaming a display (0: the main one) at width×height. 0 when it's
/// going; otherwise 1 and why in `err`.
int muxen_capture_start(uint32_t id, int fps, size_t width, size_t height, muxen_frame_fn fn, void *ctx, char *err, size_t errlen) {
    @autoreleasepool {
        dispatch_semaphore_t done = dispatch_semaphore_create(0);
        __block SCShareableContent *content = nil;
        __block NSError *failure = nil;
        [SCShareableContent getShareableContentExcludingDesktopWindows:NO
                                                   onScreenWindowsOnly:YES
                                                     completionHandler:^(SCShareableContent *c, NSError *e) {
                                                         content = c;
                                                         failure = e;
                                                         dispatch_semaphore_signal(done);
                                                     }];
        if (dispatch_semaphore_wait(done, dispatch_time(DISPATCH_TIME_NOW, 10 * NSEC_PER_SEC)) != 0) {
            say(err, errlen, @"the screen didn't answer");
            return 1;
        }
        if (!content) {
            say(err, errlen, failure ? failure.localizedDescription : @"no screen to show");
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
        SCStreamConfiguration *config = [SCStreamConfiguration new];
        config.width = width;
        config.height = height;
        config.pixelFormat = kCVPixelFormatType_32BGRA;
        config.minimumFrameInterval = CMTimeMake(1, fps > 0 ? fps : 30);
        config.showsCursor = YES;
        config.queueDepth = 3;
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
        __block NSError *start = nil;
        [muxen_stream startCaptureWithCompletionHandler:^(NSError *e) {
            start = e;
            dispatch_semaphore_signal(done);
        }];
        if (dispatch_semaphore_wait(done, dispatch_time(DISPATCH_TIME_NOW, 10 * NSEC_PER_SEC)) != 0) {
            say(err, errlen, @"the screen capture didn't start");
            return 1;
        }
        if (start) {
            say(err, errlen, start.localizedDescription);
            return 1;
        }
        return 0;
    }
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

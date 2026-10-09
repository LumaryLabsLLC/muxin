// The Mac's screen for `muxen-agent screen`: ScreenCaptureKit streams the
// main display at the size asked for, and each new frame (BGRA) goes to a
// callback in Rust (see desktop.rs). Needs Screen Recording permission.

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

/// The main display: its size in pixels and in points (what input uses).
void muxen_display(size_t *pixels_w, size_t *pixels_h, double *points_w, double *points_h) {
    CGDirectDisplayID id = CGMainDisplayID();
    CGDisplayModeRef mode = CGDisplayCopyDisplayMode(id);
    *pixels_w = mode ? CGDisplayModeGetPixelWidth(mode) : CGDisplayPixelsWide(id);
    *pixels_h = mode ? CGDisplayModeGetPixelHeight(mode) : CGDisplayPixelsHigh(id);
    if (mode) {
        CGDisplayModeRelease(mode);
    }
    CGRect bounds = CGDisplayBounds(id);
    *points_w = bounds.size.width;
    *points_h = bounds.size.height;
}

/// Starts streaming the main display at width×height. 0 when it's going;
/// otherwise 1 and why in `err`.
int muxen_capture_start(int fps, size_t width, size_t height, muxen_frame_fn fn, void *ctx, char *err, size_t errlen) {
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
        SCDisplay *display = nil;
        for (SCDisplay *d in content.displays) {
            if (d.displayID == CGMainDisplayID()) {
                display = d;
            }
        }
        if (!display) {
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

// Display capture through ScreenCaptureKit, for screen_capture.rs. The stream
// delivers NV12 (video range, BT.601) at the display's pixel size with the
// pointer drawn in, so the encoder reads the frames without a conversion.
// Compiled with ARC.

#import <CoreMedia/CoreMedia.h>
#import <CoreVideo/CoreVideo.h>
#import <ScreenCaptureKit/ScreenCaptureKit.h>
#include <string.h>

static const int64_t kWaitSeconds = 10;

static void set_error(char *error, size_t length, NSString *text) {
    if (length == 0) return;
    const char *utf8 = text.UTF8String ?: "unknown error";
    strlcpy(error, utf8, length);
}

// Keeps the newest complete frame. The stream calls it on its own queue;
// guest_capture_next takes the frame on the capture thread.
@interface GuestCaptureOutput : NSObject <SCStreamOutput, SCStreamDelegate> {
  @public
    NSCondition *condition;
    CVPixelBufferRef latest;
    NSString *stopReason;
}
@end

@implementation GuestCaptureOutput

- (instancetype)init {
    if ((self = [super init])) condition = [NSCondition new];
    return self;
}

- (void)dealloc {
    if (latest) CVPixelBufferRelease(latest);
}

- (void)stream:(SCStream *)stream
    didOutputSampleBuffer:(CMSampleBufferRef)sampleBuffer
                   ofType:(SCStreamOutputType)type {
    if (type != SCStreamOutputTypeScreen || !CMSampleBufferIsValid(sampleBuffer)) return;
    // Idle and blank frames carry no new image.
    CFArrayRef attachments = CMSampleBufferGetSampleAttachmentsArray(sampleBuffer, false);
    if (!attachments || CFArrayGetCount(attachments) == 0) return;
    NSDictionary *info = (__bridge NSDictionary *)CFArrayGetValueAtIndex(attachments, 0);
    NSNumber *status = info[SCStreamFrameInfoStatus];
    if (status == nil || status.integerValue != SCFrameStatusComplete) return;
    CVPixelBufferRef pixels = CMSampleBufferGetImageBuffer(sampleBuffer);
    if (!pixels) return;
    CVPixelBufferRetain(pixels);
    [condition lock];
    if (latest) CVPixelBufferRelease(latest);
    latest = pixels;
    [condition signal];
    [condition unlock];
}

- (void)stream:(SCStream *)stream didStopWithError:(NSError *)error {
    [condition lock];
    stopReason = error.localizedDescription ?: @"the capture stopped";
    [condition broadcast];
    [condition unlock];
}

@end

@interface GuestCapture : NSObject
@property(strong) SCStream *stream;
@property(strong) GuestCaptureOutput *output;
@property(strong) dispatch_queue_t queue;
// Under @synchronized(self): the start finished, or the caller stopped
// waiting for it.
@property BOOL startFinished;
@property BOOL abandoned;
@end

@implementation GuestCapture
@end

static SCDisplay *shareable_display(uint32_t display_id, char *error, size_t error_length) {
    __block SCShareableContent *content = nil;
    __block NSError *failure = nil;
    dispatch_semaphore_t done = dispatch_semaphore_create(0);
    [SCShareableContent
        getShareableContentExcludingDesktopWindows:NO
                               onScreenWindowsOnly:YES
                                 completionHandler:^(SCShareableContent *found, NSError *err) {
                                   content = found;
                                   failure = err;
                                   dispatch_semaphore_signal(done);
                                 }];
    if (dispatch_semaphore_wait(done, dispatch_time(DISPATCH_TIME_NOW, kWaitSeconds * NSEC_PER_SEC))) {
        set_error(error, error_length, @"ScreenCaptureKit did not list the displays");
        return nil;
    }
    if (content == nil) {
        set_error(error, error_length, failure.localizedDescription ?: @"no shareable content");
        return nil;
    }
    for (SCDisplay *display in content.displays) {
        if (display.displayID == display_id) return display;
    }
    set_error(error, error_length,
              [NSString stringWithFormat:@"display %u is not shareable", display_id]);
    return nil;
}

// Starts a stream of the display at width x height pixels, at most fps
// frames a second. Returns NULL and writes the reason into error on failure.
void *guest_capture_start(uint32_t display_id, size_t width, size_t height, int32_t fps,
                          char *error, size_t error_length) {
    @autoreleasepool {
        SCDisplay *display = shareable_display(display_id, error, error_length);
        if (display == nil) return NULL;
        SCContentFilter *filter = [[SCContentFilter alloc] initWithDisplay:display
                                                          excludingWindows:@[]];
        SCStreamConfiguration *config = [SCStreamConfiguration new];
        config.width = width;
        config.height = height;
        config.pixelFormat = kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange;
        config.colorMatrix = kCGDisplayStreamYCbCrMatrix_ITU_R_601_4;
        config.showsCursor = YES;
        config.minimumFrameInterval = CMTimeMake(1, fps);
        // The capture keeps its newest frame and the output one more, so
        // the stream writes into the other two.
        config.queueDepth = 4;

        GuestCapture *capture = [GuestCapture new];
        capture.output = [GuestCaptureOutput new];
        capture.queue = dispatch_queue_create("ipkvm-os-services.capture", DISPATCH_QUEUE_SERIAL);
        capture.stream = [[SCStream alloc] initWithFilter:filter
                                            configuration:config
                                                 delegate:capture.output];
        NSError *added = nil;
        if (![capture.stream addStreamOutput:capture.output
                                        type:SCStreamOutputTypeScreen
                          sampleHandlerQueue:capture.queue
                                       error:&added]) {
            set_error(error, error_length, added.localizedDescription ?: @"no stream output");
            return NULL;
        }
        // A start that finishes after the caller gave up stops the stream
        // then; the handler keeps the capture alive until it runs.
        __block NSError *failure = nil;
        dispatch_semaphore_t started = dispatch_semaphore_create(0);
        [capture.stream startCaptureWithCompletionHandler:^(NSError *err) {
          BOOL abandoned;
          @synchronized(capture) {
              capture.startFinished = YES;
              failure = err;
              abandoned = capture.abandoned;
          }
          if (abandoned && err == nil) {
              [capture.stream stopCaptureWithCompletionHandler:^(NSError *__unused e) {
                (void)capture;
              }];
          }
          dispatch_semaphore_signal(started);
        }];
        dispatch_semaphore_wait(started, dispatch_time(DISPATCH_TIME_NOW, kWaitSeconds * NSEC_PER_SEC));
        BOOL finished;
        @synchronized(capture) {
            finished = capture.startFinished;
            capture.abandoned = !finished;
        }
        if (!finished) {
            set_error(error, error_length, @"the capture did not start");
            return NULL;
        }
        if (failure != nil) {
            set_error(error, error_length, failure.localizedDescription);
            return NULL;
        }
        return (void *)CFBridgingRetain(capture);
    }
}

// The newest complete frame, waiting up to timeout_ms for one. Returns a
// retained pixel buffer, or NULL when no frame arrived; then *stopped is 1
// and error holds the reason if the stream ended.
void *guest_capture_next(void *handle, int32_t timeout_ms, int32_t *stopped, char *error,
                         size_t error_length) {
    @autoreleasepool {
        GuestCaptureOutput *output = ((__bridge GuestCapture *)handle).output;
        NSDate *deadline = [NSDate dateWithTimeIntervalSinceNow:timeout_ms / 1000.0];
        [output->condition lock];
        while (!output->latest && output->stopReason == nil) {
            if (![output->condition waitUntilDate:deadline]) break;
        }
        CVPixelBufferRef pixels = output->latest;
        output->latest = NULL;
        NSString *reason = output->stopReason;
        [output->condition unlock];
        *stopped = pixels == NULL && reason != nil;
        if (*stopped) set_error(error, error_length, reason);
        return pixels;
    }
}

// Copies a frame from guest_capture_next into an NV12 buffer with the given
// plane layout. Returns 0, or -1 when the frame is not width x height NV12.
int32_t guest_capture_copy(void *frame, uint8_t *dst, size_t dst_length, size_t width,
                           size_t height, size_t y_stride, size_t uv_offset, size_t uv_stride) {
    CVPixelBufferRef pixels = (CVPixelBufferRef)frame;
    if (CVPixelBufferGetPixelFormatType(pixels) != kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange ||
        CVPixelBufferGetPlaneCount(pixels) != 2 ||
        CVPixelBufferGetWidthOfPlane(pixels, 0) < width ||
        CVPixelBufferGetHeightOfPlane(pixels, 0) < height ||
        uv_offset + (height / 2) * uv_stride > dst_length || height * y_stride > uv_offset) {
        return -1;
    }
    if (CVPixelBufferLockBaseAddress(pixels, kCVPixelBufferLock_ReadOnly) != kCVReturnSuccess) {
        return -1;
    }
    const uint8_t *y = CVPixelBufferGetBaseAddressOfPlane(pixels, 0);
    const uint8_t *uv = CVPixelBufferGetBaseAddressOfPlane(pixels, 1);
    size_t y_row = CVPixelBufferGetBytesPerRowOfPlane(pixels, 0);
    size_t uv_row = CVPixelBufferGetBytesPerRowOfPlane(pixels, 1);
    for (size_t row = 0; row < height; row++) {
        memcpy(dst + row * y_stride, y + row * y_row, width);
    }
    for (size_t row = 0; row < height / 2; row++) {
        memcpy(dst + uv_offset + row * uv_stride, uv + row * uv_row, width);
    }
    CVPixelBufferUnlockBaseAddress(pixels, kCVPixelBufferLock_ReadOnly);
    return 0;
}

void guest_capture_release_frame(void *frame) {
    CVPixelBufferRelease((CVPixelBufferRef)frame);
}

// Stops the stream and frees it. A stop that takes longer than the wait
// keeps the capture alive until it finishes.
void guest_capture_stop(void *handle) {
    @autoreleasepool {
        GuestCapture *capture = CFBridgingRelease(handle);
        dispatch_semaphore_t stopped = dispatch_semaphore_create(0);
        [capture.stream stopCaptureWithCompletionHandler:^(NSError *__unused err) {
          (void)capture;
          dispatch_semaphore_signal(stopped);
        }];
        dispatch_semaphore_wait(stopped, dispatch_time(DISPATCH_TIME_NOW, kWaitSeconds * NSEC_PER_SEC));
    }
}

#import <AppKit/AppKit.h>

void guest_reopened(void);

// Opening the app while the service runs reaches the service as a reopen
// request, because macOS counts the service as the running app.
@interface GuestAppDelegate : NSObject <NSApplicationDelegate>
@end

@implementation GuestAppDelegate
- (BOOL)applicationShouldHandleReopen:(NSApplication *)sender hasVisibleWindows:(BOOL)flag {
    guest_reopened();
    return NO;
}
@end

static GuestAppDelegate *delegate;

void guest_appkit_initialize(void) {
    @autoreleasepool {
        [NSApplication sharedApplication];
        [NSApp setActivationPolicy:NSApplicationActivationPolicyProhibited];
        delegate = [[GuestAppDelegate alloc] init];
        NSApp.delegate = delegate;
    }
}

void guest_appkit_run(void) {
    @autoreleasepool {
        [NSApp run];
    }
}

void guest_appkit_stop(void) {
    // Queueing also handles a worker that exits before run has started.
    dispatch_async(dispatch_get_main_queue(), ^{
        [NSApp stop:nil];
        [NSApp postEvent:[NSEvent otherEventWithType:NSEventTypeApplicationDefined
            location:NSZeroPoint modifierFlags:0 timestamp:0 windowNumber:0
            context:nil subtype:0 data1:0 data2:0] atStart:YES];
    });
}

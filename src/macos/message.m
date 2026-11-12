#import <AppKit/AppKit.h>

void guest_show_message(const char *text, bool error) {
    @autoreleasepool {
        [NSApplication sharedApplication];
        // The bundle has no Dock icon, so the alert brings the app forward.
        [NSApp setActivationPolicy:NSApplicationActivationPolicyAccessory];
        [NSApp activateIgnoringOtherApps:YES];
        NSAlert *alert = [[NSAlert alloc] init];
        alert.messageText = @"IP-KVM OS Services";
        alert.informativeText = [NSString stringWithUTF8String:text];
        alert.alertStyle = error ? NSAlertStyleCritical : NSAlertStyleInformational;
        [alert runModal];
    }
}

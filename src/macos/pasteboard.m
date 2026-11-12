#import <AppKit/AppKit.h>

// Password managers mark secrets with org.nspasteboard.ConcealedType so
// clipboard tools do not read or forward them.
int PasteboardIsConcealed(void) {
    @autoreleasepool {
        NSArray<NSPasteboardType> *types = [[NSPasteboard generalPasteboard] types];
        return [types containsObject:@"org.nspasteboard.ConcealedType"] ? 1 : 0;
    }
}

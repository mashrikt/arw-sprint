#import <Cocoa/Cocoa.h>
#import <Carbon/Carbon.h>

static void (*fastcullOpenCallback)(const char *) = NULL;

@interface FastCullOpenHandler : NSObject
- (void)registerOpenHandler:(NSNotification *)notification;
- (void)handleOpen:(NSAppleEventDescriptor *)event reply:(NSAppleEventDescriptor *)reply;
@end

@implementation FastCullOpenHandler
- (void)registerOpenHandler:(NSNotification *)notification {
    (void)notification;
    // AppKit lazily creates its document controller while finishing launch.
    // Initialize that fallback first, then install our non-document handler.
    // Re-register at launch notifications without replacing winit's delegate.
    [NSDocumentController sharedDocumentController];
    [[NSAppleEventManager sharedAppleEventManager]
        setEventHandler:self andSelector:@selector(handleOpen:reply:)
        forEventClass:kCoreEventClass andEventID:kAEOpenDocuments];
}

- (void)handleOpen:(NSAppleEventDescriptor *)event reply:(NSAppleEventDescriptor *)reply {
    (void)reply;
    NSAppleEventDescriptor *items = [event paramDescriptorForKeyword:keyDirectObject];
    for (NSInteger i = 1; i <= [items numberOfItems]; ++i) {
        NSAppleEventDescriptor *item = [[items descriptorAtIndex:i] coerceToDescriptorType:typeFileURL];
        if (item == nil) { continue; }
        NSString *urlString = [[NSString alloc] initWithData:[item data] encoding:NSUTF8StringEncoding];
        NSURL *url = urlString == nil ? nil : [NSURL URLWithString:urlString];
        if ([url isFileURL] && fastcullOpenCallback != NULL) {
            fastcullOpenCallback([url fileSystemRepresentation]);
        }
    }
    [NSApp activateIgnoringOtherApps:YES];
}
@end

void fastcull_install_open_handler(void (*callback)(const char *)) {
    NSCAssert([NSThread isMainThread], @"FastCull open handler must initialize on the main thread");
    static FastCullOpenHandler *handler;
    fastcullOpenCallback = callback;
    if (handler == nil) {
        handler = [[FastCullOpenHandler alloc] init];
        NSNotificationCenter *notifications = [NSNotificationCenter defaultCenter];
        [notifications addObserver:handler selector:@selector(registerOpenHandler:)
                              name:NSApplicationWillFinishLaunchingNotification object:NSApp];
        [notifications addObserver:handler selector:@selector(registerOpenHandler:)
                              name:NSApplicationDidFinishLaunchingNotification object:NSApp];
    }
    [handler registerOpenHandler:nil];
}

#import <Cocoa/Cocoa.h>
#import <Carbon/Carbon.h>
#include <stdlib.h>
#include <stdio.h>
#include <string.h>

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

// A small, synchronous AppKit editor keeps preferences out of the image render
// path. The local controller owns the table's datasource for the whole modal run.
@interface FastCullShortcutEditor : NSObject <NSTableViewDataSource, NSTableViewDelegate, NSWindowDelegate>
@property(nonatomic, strong) NSPanel *window;
@property(nonatomic, strong) NSTableView *table;
@property(nonatomic, strong) NSMutableArray<NSMutableDictionary<NSString *, NSString *> *> *rows;
- (instancetype)initWithRows:(NSString *)rows error:(NSString *)error;
- (void)save:(id)sender;
- (void)cancel:(id)sender;
- (void)restoreDefaults:(id)sender;
- (NSString *)configuration;
@end

@implementation FastCullShortcutEditor
- (instancetype)initWithRows:(NSString *)rows error:(NSString *)error {
    self = [super init];
    if (self == nil) { return nil; }
    _rows = [NSMutableArray array];
    for (NSString *line in [rows componentsSeparatedByString:@"\n"]) {
        NSArray<NSString *> *fields = [line componentsSeparatedByString:@"\t"];
        if (fields.count != 3 || fields[0].length == 0) { continue; }
        [_rows addObject:[@{@"id": fields[0], @"action": fields[1], @"shortcuts": fields[2]} mutableCopy]];
    }
    _window = [[NSPanel alloc] initWithContentRect:NSMakeRect(0, 0, 760, 620)
                                       styleMask:NSWindowStyleMaskTitled | NSWindowStyleMaskClosable | NSWindowStyleMaskResizable
                                         backing:NSBackingStoreBuffered defer:NO];
    _window.title = @"Keyboard Shortcuts";
    _window.releasedWhenClosed = NO;
    _window.worksWhenModal = YES;
    _window.minSize = NSMakeSize(660, 480);
    _window.delegate = self;
    NSView *content = _window.contentView;

    NSTextField *instructions = [NSTextField wrappingLabelWithString:
        @"Double-click a shortcut to edit it. Use commas for alternatives; leave it blank to remove it.\nExamples: S, Shift+Space, Cmd+Option+5"];
    instructions.frame = NSMakeRect(20, 563, 720, 42);
    instructions.autoresizingMask = NSViewWidthSizable | NSViewMinYMargin;
    [content addSubview:instructions];

    NSScrollView *scroll = [[NSScrollView alloc] initWithFrame:NSMakeRect(20, 140, 720, 410)];
    scroll.borderType = NSBezelBorder;
    scroll.hasVerticalScroller = YES;
    scroll.autoresizingMask = NSViewWidthSizable | NSViewHeightSizable;
    _table = [[NSTableView alloc] initWithFrame:scroll.bounds];
    _table.usesAlternatingRowBackgroundColors = YES;
    _table.rowHeight = 26;
    _table.allowsColumnReordering = NO;
    _table.columnAutoresizingStyle = NSTableViewLastColumnOnlyAutoresizingStyle;
    NSTableColumn *action = [[NSTableColumn alloc] initWithIdentifier:@"action"];
    action.title = @"Action";
    action.width = 320;
    action.minWidth = 240;
    action.editable = NO;
    [_table addTableColumn:action];
    NSTableColumn *shortcuts = [[NSTableColumn alloc] initWithIdentifier:@"shortcuts"];
    shortcuts.title = @"Shortcuts";
    shortcuts.width = 380;
    shortcuts.minWidth = 250;
    shortcuts.editable = YES;
    [_table addTableColumn:shortcuts];
    _table.dataSource = self;
    _table.delegate = self;
    scroll.documentView = _table;
    [content addSubview:scroll];

    NSTextField *message = [NSTextField wrappingLabelWithString:error ?: @""];
    message.frame = NSMakeRect(20, 72, 720, 57);
    message.textColor = NSColor.systemRedColor;
    message.autoresizingMask = NSViewWidthSizable;
    message.selectable = YES;
    [content addSubview:message];

    NSButton *restore = [NSButton buttonWithTitle:@"Restore Defaults" target:self action:@selector(restoreDefaults:)];
    restore.frame = NSMakeRect(16, 20, 150, 32);
    [content addSubview:restore];
    NSButton *cancel = [NSButton buttonWithTitle:@"Cancel" target:self action:@selector(cancel:)];
    cancel.frame = NSMakeRect(542, 20, 96, 32);
    cancel.autoresizingMask = NSViewMinXMargin;
    cancel.keyEquivalent = @"\033";
    [content addSubview:cancel];
    NSButton *save = [NSButton buttonWithTitle:@"Save" target:self action:@selector(save:)];
    save.frame = NSMakeRect(646, 20, 96, 32);
    save.autoresizingMask = NSViewMinXMargin;
    save.keyEquivalent = @"\r";
    [content addSubview:save];
    _window.defaultButtonCell = save.cell;
    [_window center];
    return self;
}

- (NSInteger)numberOfRowsInTableView:(NSTableView *)tableView {
    (void)tableView;
    return (NSInteger)self.rows.count;
}

- (id)tableView:(NSTableView *)tableView objectValueForTableColumn:(NSTableColumn *)column row:(NSInteger)row {
    (void)tableView;
    if (row < 0 || (NSUInteger)row >= self.rows.count) { return @""; }
    return self.rows[(NSUInteger)row][column.identifier] ?: @"";
}

- (BOOL)tableView:(NSTableView *)tableView shouldEditTableColumn:(NSTableColumn *)column row:(NSInteger)row {
    (void)tableView;
    return row >= 0 && (NSUInteger)row < self.rows.count && [column.identifier isEqualToString:@"shortcuts"];
}

- (void)tableView:(NSTableView *)tableView setObjectValue:(id)value forTableColumn:(NSTableColumn *)column row:(NSInteger)row {
    (void)tableView;
    if (row < 0 || (NSUInteger)row >= self.rows.count || ![column.identifier isEqualToString:@"shortcuts"]) { return; }
    if (![value isKindOfClass:NSString.class]) { return; }
    self.rows[(NSUInteger)row][@"shortcuts"] = value;
}

- (void)save:(id)sender {
    (void)sender;
    // Commit the field editor to the datasource before Rust receives the draft.
    if ([self.window makeFirstResponder:nil]) {
        [NSApp stopModalWithCode:NSModalResponseOK];
    }
}

- (void)cancel:(id)sender {
    (void)sender;
    [NSApp stopModalWithCode:NSModalResponseCancel];
}

- (void)restoreDefaults:(id)sender {
    (void)sender;
    [NSApp stopModalWithCode:NSModalResponseStop];
}

- (BOOL)windowShouldClose:(NSWindow *)sender {
    (void)sender;
    [self cancel:nil];
    return NO;
}

- (NSString *)configuration {
    NSMutableString *configuration = [NSMutableString string];
    for (NSDictionary<NSString *, NSString *> *row in self.rows) {
        [configuration appendFormat:@"%@=%@\n", row[@"id"], row[@"shortcuts"]];
    }
    return configuration;
}
@end

char *fastcull_edit_shortcuts(const char *rows, const char *error) {
    if (![NSThread isMainThread] || rows == NULL) { return NULL; }
    @autoreleasepool {
        NSString *rowText = [[NSString alloc] initWithUTF8String:rows];
        NSString *errorText = error == NULL ? nil : [[NSString alloc] initWithUTF8String:error];
        if (rowText == nil || (error != NULL && errorText == nil)) { return NULL; }
        FastCullShortcutEditor *editor = [[FastCullShortcutEditor alloc] initWithRows:rowText error:errorText];
        if (editor == nil) { return NULL; }
        [editor.window makeKeyAndOrderFront:nil];
        NSModalResponse response = [NSApp runModalForWindow:editor.window];
        [editor.window orderOut:nil];
        [editor.window close];
        NSString *configuration = nil;
        if (response == NSModalResponseOK) { configuration = [editor configuration]; }
        if (response == NSModalResponseStop) { configuration = @""; }
        const char *utf8 = configuration.UTF8String;
        return utf8 == NULL ? NULL : strdup(utf8);
    }
}

void fastcull_free_shortcut_result(char *result) {
    free(result);
}

// Smoke-test input targets this app only. ANSI letters/punctuation are sufficient
// for the documented US-layout smoke fixture; this is not a global input hook.
void fastcull_post_shortcut_test_key(const char *character, uint8_t command, uint8_t shift) {
    if (![NSThread isMainThread] || character == NULL) { return; }
    @autoreleasepool {
        NSString *text = [[NSString alloc] initWithUTF8String:character];
        // Fullscreen transitions or another foreground app can leave keyWindow
        // nil during the opt-in smoke run. Target and activate our own main
        // window explicitly so the diagnostic tests routing, not focus timing.
        NSWindow *window = NSApp.keyWindow ?: NSApp.mainWindow;
        if (window == nil) {
            for (NSWindow *candidate in NSApp.windows) {
                if (candidate.isVisible && ![candidate isKindOfClass:NSPanel.class]) {
                    window = candidate;
                    break;
                }
            }
        }
        if (text.length != 1 || window == nil) {
            fprintf(stderr, "SMOKE NATIVE KEY: no own window available\n");
            return;
        }
        [NSApp activateIgnoringOtherApps:YES];
        [window makeKeyAndOrderFront:nil];
        fprintf(stderr, "SMOKE NATIVE KEY: target=%ld key=%d active=%d\n",
                (long)window.windowNumber, window.isKeyWindow, NSApp.isActive);
        NSString *keys = @"asdfhgzxcv\u00a7bqweryt123465=97-80]ou[ip\rlj'k;\\,/nm.\t `";
        NSRange location = [keys rangeOfString:text.lowercaseString];
        if (location.location == NSNotFound) { return; }
        NSEventModifierFlags modifiers = 0;
        if (command != 0) { modifiers |= NSEventModifierFlagCommand; }
        if (shift != 0) { modifiers |= NSEventModifierFlagShift; }
        NSString *characters = shift == 0 ? text : text.uppercaseString;
        for (NSUInteger i = 0; i < 2; ++i) {
            NSEvent *event = [NSEvent keyEventWithType:i == 0 ? NSEventTypeKeyDown : NSEventTypeKeyUp
                                             location:NSZeroPoint modifierFlags:modifiers
                                             timestamp:NSProcessInfo.processInfo.systemUptime
                                             windowNumber:window.windowNumber context:nil
                                             characters:characters charactersIgnoringModifiers:text
                                             isARepeat:NO keyCode:(unsigned short)location.location];
            if (event != nil) { [NSApp postEvent:event atStart:NO]; }
        }
    }
}

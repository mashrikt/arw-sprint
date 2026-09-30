#import <Cocoa/Cocoa.h>
#include <stdint.h>

// Offscreen only. NSGraphicsContext is thread-local; no view or window is touched.
// A linear-sRGB premultiplied bitmap permits correct, cheap GPU alpha blending.
int fastcull_draw_photo_details(const char *content, uint8_t *rgba,
                               uint32_t width, uint32_t height, double scale) {
    if (!content || !rgba || width == 0 || height == 0 ||
        width > 2048 || height > 2048 || !isfinite(scale) || scale < 1 || scale > 4) {
        return 0;
    }
    @autoreleasepool {
        NSString *text = [NSString stringWithUTF8String:content];
        if (!text) return 0;
        CGColorSpaceRef space = CGColorSpaceCreateWithName(kCGColorSpaceLinearSRGB);
        if (!space) return 0;
        CGContextRef bitmap = CGBitmapContextCreate(rgba, width, height, 8,
            (size_t)width * 4, space,
            kCGImageAlphaPremultipliedLast | kCGBitmapByteOrder32Big);
        CGColorSpaceRelease(space);
        if (!bitmap) return 0;
        CGContextTranslateCTM(bitmap, 0, height);
        CGContextScaleCTM(bitmap, scale, -scale);
        CGContextSetShouldAntialias(bitmap, true);
        [NSGraphicsContext saveGraphicsState];
        [NSGraphicsContext setCurrentContext:
            [NSGraphicsContext graphicsContextWithCGContext:bitmap flipped:YES]];

        CGFloat w = width / scale, h = height / scale;
        NSRect card = NSMakeRect(2, 2, w - 4, h - 4);
        NSBezierPath *outline = [NSBezierPath bezierPathWithRoundedRect:card
            xRadius:15 yRadius:15];
        // The tint remains readable above both white skies and dark subjects.
        NSGradient *tint = [[NSGradient alloc] initWithStartingColor:
            [NSColor colorWithSRGBRed:0.18 green:0.19 blue:0.21 alpha:0.90]
            endingColor:[NSColor colorWithSRGBRed:0.075 green:0.085 blue:0.10 alpha:0.90]];
        [tint drawInBezierPath:outline angle:90];
        [[NSColor colorWithSRGBRed:0.9 green:0.93 blue:1 alpha:0.25] setStroke];
        outline.lineWidth = 1 / scale;
        [outline stroke];

        NSMutableParagraphStyle *single = [[NSMutableParagraphStyle alloc] init];
        single.lineBreakMode = NSLineBreakByTruncatingTail;
        NSColor *muted = [NSColor colorWithSRGBRed:0.69 green:0.73 blue:0.79 alpha:1];
        NSColor *white = [NSColor colorWithSRGBRed:0.97 green:0.98 blue:1 alpha:1];
        NSDictionary *heading = @{
            NSFontAttributeName: [NSFont systemFontOfSize:10 weight:NSFontWeightSemibold],
            NSForegroundColorAttributeName: muted,
            NSParagraphStyleAttributeName: single,
            NSKernAttributeName: @1.1
        };
        NSDictionary *filename = @{
            NSFontAttributeName: [NSFont systemFontOfSize:15 weight:NSFontWeightSemibold],
            NSForegroundColorAttributeName: white,
            NSParagraphStyleAttributeName: single
        };
        CGFloat labelWidth = w < 260 ? 66 : 85;
        CGFloat fontSize = w < 260 ? 11 : 12;
        NSDictionary *label = @{
            NSFontAttributeName: [NSFont systemFontOfSize:fontSize],
            NSForegroundColorAttributeName: muted,
            NSParagraphStyleAttributeName: single
        };
        NSDictionary *value = @{
            NSFontAttributeName: [NSFont systemFontOfSize:fontSize weight:NSFontWeightMedium],
            NSForegroundColorAttributeName: white,
            NSParagraphStyleAttributeName: single
        };
        NSMutableParagraphStyle *wrapped = [single mutableCopy];
        wrapped.lineBreakMode = NSLineBreakByWordWrapping;
        NSMutableDictionary *lensValue = [value mutableCopy];
        lensValue[NSParagraphStyleAttributeName] = wrapped;
        NSArray<NSString *> *lines = [text componentsSeparatedByString:@"\n"];
        [@"PHOTO DETAILS" drawInRect:NSMakeRect(18, 15, w - 36, 14) withAttributes:heading];
        [lines.firstObject drawInRect:NSMakeRect(18, 34, w - 36, 21) withAttributes:filename];
        [[NSColor colorWithWhite:1 alpha:0.14] setFill];
        NSRectFill(NSMakeRect(18, 64, w - 36, 1 / scale));
        CGFloat y = 78;
        NSUInteger rowCount = MAX((NSUInteger)1, lines.count - 1);
        BOOL hasLens = [text containsString:@"\nLens\t"];
        CGFloat lensExtra = hasLens && h >= 94 + rowCount * 18 + 18 ? 18 : 0;
        CGFloat step = MIN(27, MAX(18, (h - 94 - lensExtra) / rowCount));
        BOOL allFit = 94 + rowCount * step + lensExtra <= h;
        for (NSUInteger i = 1; i < lines.count; ++i) {
            if (y + 18 > h - (allFit ? 12 : 36)) {
                [@"Resize for more details" drawInRect:NSMakeRect(18, h - 28, w - 36, 18)
                    withAttributes:label];
                break;
            }
            NSString *line = lines[i];
            NSRange tab = [line rangeOfString:@"\t"];
            if (tab.location == NSNotFound) {
                [line drawInRect:NSMakeRect(18, y, w - 36, 20) withAttributes:label];
            } else {
                NSString *labelText = [line substringToIndex:tab.location];
                BOOL lens = [labelText isEqualToString:@"Lens"] && lensExtra > 0;
                [labelText
                    drawInRect:NSMakeRect(18, y, labelWidth - 7, 20) withAttributes:label];
                [[line substringFromIndex:tab.location + 1]
                    drawInRect:NSMakeRect(18 + labelWidth, y, w - 36 - labelWidth, lens ? 36 : 20)
                    withAttributes:lens ? lensValue : value];
                if (lens) y += lensExtra;
            }
            y += step;
        }
        [NSGraphicsContext restoreGraphicsState];
        CGContextRelease(bitmap);
    }
    return 1;
}

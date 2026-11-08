#import <Foundation/Foundation.h>
#import <ColorSync/ColorSync.h>
#import <string.h>

// Called on the JPEG/thumbnail worker. No file access or full-image copy.
// One bounded row keeps conversion inside the existing decoded allocation.
int fastcull_jpeg_to_srgb(const uint8_t *profile, size_t profile_len,
                         uint8_t adobe_rgb, uint8_t *rgba,
                         uint32_t width, uint32_t height) {
    if (!rgba || width == 0 || height == 0 || width > 16384 || height > 16384 ||
        (uint64_t)width * height * 4 > 256ULL * 1024 * 1024 || profile_len > 1024 * 1024) return 0;
    @autoreleasepool {
        ColorSyncProfileRef source = NULL;
        if (profile_len) {
            if (!profile || profile_len < 128 || memcmp(profile + 16, "RGB ", 4) != 0) return 0;
            CFDataRef data = CFDataCreate(kCFAllocatorDefault, profile, (CFIndex)profile_len);
            if (data) { source = ColorSyncProfileCreate(data, NULL); CFRelease(data); }
        } else if (adobe_rgb) {
            source = ColorSyncProfileCreateWithName(kColorSyncAdobeRGB1998Profile);
        }
        if (!source) return 0;
        ColorSyncProfileRef target = ColorSyncProfileCreateWithName(kColorSyncSRGBProfile);
        if (!target) { CFRelease(source); return 0; }
        if (CFEqual(source, target)) { CFRelease(source); CFRelease(target); return 1; }
        NSDictionary *from = @{(__bridge NSString *)kColorSyncProfile: (__bridge id)source,
            (__bridge NSString *)kColorSyncRenderingIntent: (__bridge NSString *)kColorSyncRenderingIntentRelative,
            (__bridge NSString *)kColorSyncTransformTag: (__bridge NSString *)kColorSyncTransformDeviceToPCS};
        NSDictionary *to = @{(__bridge NSString *)kColorSyncProfile: (__bridge id)target,
            (__bridge NSString *)kColorSyncRenderingIntent: (__bridge NSString *)kColorSyncRenderingIntentRelative,
            (__bridge NSString *)kColorSyncTransformTag: (__bridge NSString *)kColorSyncTransformPCSToDevice};
        ColorSyncTransformRef transform = ColorSyncTransformCreate((__bridge CFArrayRef)@[from, to], NULL);
        CFRelease(source);
        CFRelease(target);
        if (!transform) return 0;
        uint8_t row[16384 * 4];
        size_t stride = (size_t)width * 4;
        bool ok = true;
        for (uint32_t y = 0; y < height && ok; ++y) {
            uint8_t *input = rgba + y * stride;
            ok = ColorSyncTransformConvert(transform, width, 1, row, kColorSync8BitInteger,
                kColorSyncAlphaLast, stride, input, kColorSync8BitInteger,
                kColorSyncAlphaLast, stride, NULL);
            if (ok) memcpy(input, row, stride);
        }
        CFRelease(transform);
        return ok ? 1 : 0;
    }
}

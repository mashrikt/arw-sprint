// A small, owned LibRaw bridge. Only this explicitly requested path reads sensor data.
#include "libraw/libraw.h"
#include <algorithm>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <exception>
#include <new>
#include <sys/stat.h>

struct FastCullRawInfo {
    uint32_t width, height, orientation, reserved;
    uint64_t scratch_bytes;
};
using Cancel = int (*)(void *);
struct RawHandle {
    LibRaw decoder;
    FastCullRawInfo info{};
    Cancel cancel = nullptr;
    void *cancel_context = nullptr;
    bool opened = false;
    uint64_t metadata_bytes = 0;
    uint32_t metadata_entries = 0;
    bool metadata_rejected = false;
};
static constexpr uint64_t MiB = 1024 * 1024;
static constexpr uint64_t MaxScratch = 768 * MiB;
static void error_text(char *dst, size_t capacity, const char *message) {
    if (dst && capacity) snprintf(dst, capacity, "%s", message);
}
static int progress(void *context, enum LibRaw_progress, int, int) {
    auto *h = static_cast<RawHandle *>(context);
    return h->cancel && h->cancel(h->cancel_context);
}
// LibRaw invokes these hooks before interpreting TIFF/EXIF/MakerNote entries.
// Bound declared metadata before native parsing allocates it. Sensor strips are
// referenced by scalar offsets/counts, so these limits do not cap sensor data.
static void metadata(void *context, int, int type, int count,
                     unsigned int, void *, INT64) {
    auto *h = static_cast<RawHandle *>(context);
    static constexpr unsigned sizes[19] = {0,1,1,2,4,8,1,1,2,4,8,4,8,4,0,0,8,8,8};
    const unsigned unit = type >= 0 && type < 19 ? sizes[type] : 0;
    const uint64_t bytes = count >= 0 ? uint64_t(count) * unit : UINT64_MAX;
    if (!unit || count < 0 || bytes > 8 * MiB ||
        h->metadata_entries >= 16384 || bytes > 16 * MiB - h->metadata_bytes) {
        h->metadata_rejected = true;
        throw LIBRAW_EXCEPTION_IO_CORRUPT;
    }
    ++h->metadata_entries;
    h->metadata_bytes += bytes;
}
static bool cancelled(RawHandle *h) {
    return h->cancel && h->cancel(h->cancel_context);
}
static uint32_t orientation(int flip) {
    // LibRaw's bitwise transpose/flip convention to TIFF's orientation values.
    static constexpr uint32_t orientations[8] = {1, 2, 4, 3, 5, 8, 6, 7};
    return flip >= 0 && flip < 8 ? orientations[flip] : 1;
}
static bool valid_layout(uint32_t width, uint32_t height) {
    return width && height && width <= 16384 && height <= 16384 &&
           uint64_t(width) * height * 4 <= 256 * MiB;
}
extern "C" void *fastcull_raw_create() noexcept {
    try { return new RawHandle; } catch (...) { return nullptr; }
}
extern "C" void fastcull_raw_destroy(void *opaque) noexcept {
    delete static_cast<RawHandle *>(opaque);
}
extern "C" void fastcull_raw_reset(void *opaque) noexcept {
    if (!opaque) return;
    auto *h = static_cast<RawHandle *>(opaque);
    h->decoder.recycle(); h->opened = false; h->cancel = nullptr; h->cancel_context = nullptr;
    h->metadata_bytes = 0; h->metadata_entries = 0; h->metadata_rejected = false;
}
extern "C" int fastcull_raw_probe(void *opaque, const char *path,
        FastCullRawInfo *out, char *error, size_t error_capacity) noexcept {
    auto *h = static_cast<RawHandle *>(opaque);
    if (!h || !path || !out) return 0;
    fastcull_raw_reset(h);
    try {
        struct stat statbuf{};
        if (stat(path, &statbuf) || !S_ISREG(statbuf.st_mode) || statbuf.st_size <= 0 ||
            uint64_t(statbuf.st_size) > 1024 * MiB) {
            error_text(error, error_capacity, "RAW input is not a regular file within the 1 GiB limit");
            return 0;
        }
        auto &raw = h->decoder;
        // LibRaw uses bounded FILE-backed input by default, not an entire-file copy.
        raw.imgdata.rawparams.max_raw_memory_mb = 384;
        raw.imgdata.rawparams.use_rawspeed = 0;
        raw.imgdata.rawparams.use_dngsdk = 0;
        auto &p = raw.imgdata.params;
        // recycle() retains processing preferences; a previous camera crop must
        // never leak into the next photograph.
        for (unsigned &part : p.cropbox) part = UINT32_MAX;
        p.half_size = 0;
        p.use_camera_wb = 1;
        p.use_auto_wb = 0;
        p.no_auto_bright = 1;
        p.output_color = 1; // sRGB primaries
        p.output_bps = 8;
        p.gamm[0] = 1.0 / 2.4; p.gamm[1] = 12.92; // sRGB transfer curve
        p.user_qual = 2; // PPG: measured full-resolution detail with predictable CPU cost.
        p.user_flip = 0; // Orientation remains an inexpensive renderer transform.
        p.use_fuji_rotate = 0;
        raw.set_progress_handler(progress, h);
        raw.set_exifparser_handler(metadata, h);
        raw.set_makernotes_handler(metadata, h);
        const int status = raw.open_file(path);
        if (status != LIBRAW_SUCCESS) {
            error_text(error, error_capacity, h->metadata_rejected ?
                "RAW metadata exceeds supported limits" : libraw_strerror(status));
            raw.recycle(); return 0;
        }
        const auto &s = raw.imgdata.sizes;
        const auto &id = raw.imgdata.idata;
        if (!id.raw_count || (!id.dng_version && strcmp(id.make, "Sony") != 0) ||
            !valid_layout(s.width, s.height) || !valid_layout(s.raw_width, s.raw_height) ||
            (id.colors != 1 && id.colors != 3) || id.is_foveon ||
            s.raw_pitch > uint32_t(s.raw_width) * 8) {
            error_text(error, error_capacity, "Unsupported RAW sensor layout or dimensions");
            raw.recycle(); return 0;
        }
        // One unpacked sensor allocation + four-channel 16-bit processing image.
        // Unfamiliar codecs get the full bounded unpack allowance as well.
        const uint64_t sensor = uint64_t(s.raw_width) * s.raw_height *
            ((id.filters && !id.dng_version) ? 2 : 8);
        const uint64_t image = uint64_t(s.width) * s.height * 8;
        const char *codec = raw.unpack_function_name();
        const bool measured_sony = codec &&
            (!strcmp(codec, "sony_ljpeg_load_raw()") ||
             !strcmp(codec, "sony_arw2_load_raw()") ||
             !strcmp(codec, "unpacked_load_raw()"));
        uint64_t scratch = sensor + image + 32 * MiB;
        // Unpack temporary buffers are released before the processing image is
        // allocated, so their peak is an alternative, not an additional image.
        if (!measured_sony) scratch = std::max(scratch, 384 * MiB);
        if (scratch > MaxScratch) {
            error_text(error, error_capacity, "RAW working memory exceeds the 768 MiB limit");
            raw.recycle(); return 0;
        }
        // Honor the camera's active crop so RAW and JPEG cover the same view.
        // All crop arithmetic is widened and validated before passing to LibRaw.
        uint32_t output_width = s.width, output_height = s.height;
        const auto &crop = s.raw_inset_crops[0];
        if (crop.cwidth && crop.cheight &&
            uint32_t(crop.cleft) + crop.cwidth <= s.width &&
            uint32_t(crop.ctop) + crop.cheight <= s.height) {
            p.cropbox[0] = crop.cleft; p.cropbox[1] = crop.ctop;
            p.cropbox[2] = crop.cwidth; p.cropbox[3] = crop.cheight;
            output_width = crop.cwidth; output_height = crop.cheight;
        }
        h->info = {output_width, output_height, orientation(s.flip), 0, scratch};
        *out = h->info; h->opened = true;
        return 1;
    } catch (const std::exception &e) {
        error_text(error, error_capacity, e.what());
    } catch (...) {
        error_text(error, error_capacity, "RAW metadata decoder rejected the file");
    }
    fastcull_raw_reset(h); return 0;
}
extern "C" int fastcull_raw_decode(void *opaque, uint8_t *pixels, size_t capacity,
        Cancel cancel, void *context, char *error, size_t error_capacity) noexcept {
    auto *h = static_cast<RawHandle *>(opaque);
    if (!h || !h->opened || !pixels ||
        capacity != uint64_t(h->info.width) * h->info.height * 4) {
        error_text(error, error_capacity, "RAW output buffer does not match the probed layout");
        return 0;
    }
    h->cancel = cancel; h->cancel_context = context;
    try {
        auto &raw = h->decoder;
        int status = cancelled(h) ? LIBRAW_CANCELLED_BY_CALLBACK : raw.unpack();
        if (!status && raw.error_count()) status = LIBRAW_DATA_ERROR;
        if (!status && cancelled(h)) status = LIBRAW_CANCELLED_BY_CALLBACK;
        if (!status) status = raw.dcraw_process();
        if (!status && raw.error_count()) status = LIBRAW_DATA_ERROR;
        if (!status && cancelled(h)) status = LIBRAW_CANCELLED_BY_CALLBACK;
        if (status) {
            error_text(error, error_capacity, libraw_strerror(status));
            fastcull_raw_reset(h); return 0;
        }
        int width = 0, height = 0, colors = 0, bits = 0;
        raw.get_mem_image_format(&width, &height, &colors, &bits);
        if (width != int(h->info.width) || height != int(h->info.height) ||
            (colors != 3 && colors != 1) || bits != 8) {
            error_text(error, error_capacity, "RAW output layout changed after sensor decoding");
            fastcull_raw_reset(h); return 0;
        }
        // LibRaw writes RGB into RGBA-strided rows. Expand each row backwards
        // in place, avoiding another full-resolution RGB allocation and copy.
        status = raw.copy_mem_image(pixels, width * 4, 0);
        if (status) {
            error_text(error, error_capacity, libraw_strerror(status));
            fastcull_raw_reset(h); return 0;
        }
        for (int y = 0; y < height; ++y) {
            if (!(y % 64) && cancelled(h)) {
                error_text(error, error_capacity, "RAW decoding cancelled");
                fastcull_raw_reset(h); return 0;
            }
            uint8_t *row = pixels + size_t(y) * width * 4;
            for (int x = width - 1; x >= 0; --x) {
                const uint8_t red = row[x * colors];
                const uint8_t green = colors == 3 ? row[x * 3 + 1] : red;
                const uint8_t blue = colors == 3 ? row[x * 3 + 2] : red;
                row[x * 4] = red; row[x * 4 + 1] = green;
                row[x * 4 + 2] = blue; row[x * 4 + 3] = 255;
            }
        }
        fastcull_raw_reset(h); return 1;
    } catch (const std::exception &e) {
        error_text(error, error_capacity, e.what());
    } catch (...) {
        error_text(error, error_capacity, "RAW sensor decoder rejected the file");
    }
    fastcull_raw_reset(h); return 0;
}

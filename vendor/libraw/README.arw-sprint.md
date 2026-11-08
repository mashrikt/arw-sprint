# LibRaw used by ARW Sprint

Unmodified source from [LibRaw](https://github.com/LibRaw/LibRaw), pinned to commit
`0b87f5c907d7ee0e7641dd1b6a6c9140b9451922` (202611 snapshot, 4 November 2026).
This snapshot includes Sony ILCE-7M5 (A7 V) decoding.

ARW Sprint uses LibRaw under CDDL 1.0; see LICENSE.CDDL and COPYRIGHT.
The upstream alternative LGPL license is retained too. Source is available in
this directory and at https://github.com/mashrikt/arw-sprint/tree/master/vendor/libraw.

Build: static Intel x86_64, macOS 10.15 minimum, system zlib. No OpenMP,
RawSpeed, DNG SDK, LCMS, or external JPEG dependency. Lossy JPEG-compressed DNG
RAW decoding therefore is not enabled; its embedded JPEG can still be browsed.
The native bridge is src/platform/raw.cpp. Native source files are unmodified.

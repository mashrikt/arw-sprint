#[cfg(feature = "desktop")]
fn main() {
    assert_eq!(std::env::var("CARGO_CFG_TARGET_OS").as_deref(), Ok("macos"));
    assert_eq!(
        std::env::var("CARGO_CFG_TARGET_ARCH").as_deref(),
        Ok("x86_64")
    );
    cc::Build::new()
        .file("src/platform/macos.m")
        .file("src/platform/photo_details.m")
        .file("src/platform/jpeg_color.m")
        .flag("-fobjc-arc")
        .flag("-mmacosx-version-min=10.15")
        .compile("fastcull_macos");
    // Pin and compile the RAW engine into the Intel application. The source
    // snapshot includes A7 V compression support; no Homebrew library is needed.
    let mut raw = cc::Build::new();
    raw.cpp(true)
        .std("c++11")
        .include("vendor/libraw")
        .file("src/platform/raw.cpp")
        .define("USE_ZLIB", None)
        .define("LIBRAW_MAX_PROFILE_SIZE_MB", "4")
        .flag("-mmacosx-version-min=10.15")
        .opt_level(2)
        .warnings(false);
    for source in include_str!("vendor/libraw/sources.txt").lines() {
        raw.file(format!("vendor/libraw/{source}"));
    }
    raw.compile("fastcull_raw");
    println!("cargo:rustc-link-lib=z");
    println!("cargo:rustc-link-lib=framework=ColorSync");
    println!("cargo:rustc-link-lib=framework=Cocoa");
    println!("cargo:rustc-link-lib=framework=Carbon");
    println!("cargo:rerun-if-changed=src/platform/macos.m");
    println!("cargo:rerun-if-changed=src/platform/photo_details.m");
    println!("cargo:rerun-if-changed=src/platform/jpeg_color.m");
    println!("cargo:rerun-if-changed=src/platform/raw.cpp");
    println!("cargo:rerun-if-changed=vendor/libraw");
}

#[cfg(not(feature = "desktop"))]
fn main() {}

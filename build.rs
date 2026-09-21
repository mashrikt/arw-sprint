#[cfg(feature = "desktop")]
fn main() {
    assert_eq!(std::env::var("CARGO_CFG_TARGET_OS").as_deref(), Ok("macos"));
    assert_eq!(
        std::env::var("CARGO_CFG_TARGET_ARCH").as_deref(),
        Ok("x86_64")
    );
    cc::Build::new()
        .file("src/platform/macos.m")
        .flag("-fobjc-arc")
        .flag("-mmacosx-version-min=10.15")
        .compile("fastcull_macos");
    println!("cargo:rustc-link-lib=framework=Cocoa");
    println!("cargo:rustc-link-lib=framework=Carbon");
    println!("cargo:rerun-if-changed=src/platform/macos.m");
}

#[cfg(not(feature = "desktop"))]
fn main() {}

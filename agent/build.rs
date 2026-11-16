//! On macOS, the screen capture and input for the desktop view are a little
//! Objective-C (ScreenCaptureKit, CoreGraphics events): src/mac_screen.m.
//! And the Mac's sleep and wake are a little C (IOKit): src/mac_power.c.

fn main() {
    println!("cargo:rerun-if-changed=src/mac_screen.m");
    println!("cargo:rerun-if-changed=src/mac_power.c");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    cc::Build::new().file("src/mac_screen.m").flag("-fobjc-arc").flag("-mmacosx-version-min=12.3").compile("muxen_screen");
    cc::Build::new().file("src/mac_power.c").flag("-mmacosx-version-min=12.3").compile("muxen_power");
    for framework in ["AppKit", "ApplicationServices", "Carbon", "CoreFoundation", "CoreGraphics", "CoreMedia", "CoreVideo", "Foundation", "IOKit"] {
        println!("cargo:rustc-link-lib=framework={framework}");
    }
    // Weak: the agent still runs (without a desktop) on macOS before 12.3.
    println!("cargo:rustc-link-arg=-Wl,-weak_framework,ScreenCaptureKit");
}

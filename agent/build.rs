//! On macOS, the screen capture and input for the desktop view are a little
//! Objective-C (ScreenCaptureKit, CoreGraphics events): src/mac_screen.m.
//! And the Mac's sleep and wake are a little C (IOKit): src/mac_power.c.

fn main() {
    println!("cargo:rerun-if-changed=src/mac_screen.m");
    println!("cargo:rerun-if-changed=src/mac_power.c");
    // Windows on Arm is built for aarch64-pc-windows-gnullvm, linked through
    // Zig (scripts/build-agents.sh). From Rust 1.100 its std no longer asks
    // for libunwind itself, leaving that to a clang that adds it, and Zig's
    // doesn't: the link failed on undefined _Unwind_* symbols. Asked for
    // here; with an older Rust it's just asked for twice.
    let var = |name: &str| std::env::var(name).unwrap_or_default();
    if var("CARGO_CFG_TARGET_OS") == "windows" && var("CARGO_CFG_TARGET_ENV") == "gnu" && var("CARGO_CFG_TARGET_ABI") == "llvm" {
        println!("cargo:rustc-link-lib=unwind");
    }
    if var("CARGO_CFG_TARGET_OS") != "macos" {
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

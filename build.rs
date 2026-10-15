//! Windows: the app's icon (resource 1, which GPUI gives the window) and
//! its name in the executable's properties. Nothing to do elsewhere.

fn main() {
    println!("cargo:rerun-if-changed=assets/brand/muxin.ico");
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon_with_id("assets/brand/muxin.ico", "1");
        res.set("ProductName", "Muxin");
        res.set("FileDescription", "Muxin");
        res.set("CompanyName", "Lumary Labs");
        if let Err(e) = res.compile() {
            println!("cargo:warning=couldn't embed the icon: {e}");
        }
    }
}

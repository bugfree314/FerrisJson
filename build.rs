fn main() {
    println!("cargo:rerun-if-changed=assets/FerrisJson.ico");
    // Use the target OS, so ordinary Linux/macOS builds need no resource compiler.
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() == "windows" {
        winresource::WindowsResource::new()
            .set_icon("assets/FerrisJson.ico")
            .set("ProductName", "FerrisJson")
            .set("FileDescription", "FerrisJson - JSON Viewer")
            .compile()
            .expect("Failed to embed FerrisJson Windows icon; install Windows SDK build tools");
    }
}

fn main() {
    println!("cargo:rerun-if-changed=assets/branding/quikscope.rc");
    println!("cargo:rerun-if-changed=assets/branding/quikscope.ico");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS");
    if target_os.as_deref() == Ok("windows") {
        // llvm-rc runs beside the .rc file, while MSVC rc.exe runs in the crate.
        // An absolute macro works in both without relying on their search paths.
        let icon = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("assets/branding/quikscope.ico")
            .to_string_lossy()
            .replace('\\', "/");
        embed_resource::compile_for(
            "assets/branding/quikscope.rc",
            ["quikscope"],
            [format!("QUIKSCOPE_ICON_PATH=\"{icon}\"")],
        )
        .manifest_required()
        .expect("embedding the quikscope application icon");
    }
    if target_os.as_deref() == Ok("macos") {
        // The app bundles FFmpeg's @rpath dylibs in Contents/Frameworks
        // (packaging/macos/assemble-app.sh).
        println!("cargo:rustc-link-arg-bins=-Wl,-rpath,@executable_path/../Frameworks");
    }
}

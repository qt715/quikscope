fn main() {
    println!("cargo:rerun-if-changed=assets/branding/quikscope.rc");
    println!("cargo:rerun-if-changed=assets/branding/quikscope.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
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
}

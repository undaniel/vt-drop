fn main() {
    println!("cargo:rerun-if-changed=assets/vtdrop.rc");
    println!("cargo:rerun-if-changed=assets/vtdrop-gnu.rc");
    println!("cargo:rerun-if-changed=assets/vtdrop.manifest");
    println!("cargo:rerun-if-changed=assets/vtdrop.ico");

    // MinGW-W64 14 links a default longPathAware manifest unconditionally.
    // Embedding another manifest produces two non-default manifests and the
    // linker silently drops one. For GNU we only embed the icon and enable
    // PerMonitorV2 programmatically before creating the window. MSVC gets the
    // full manifest (DPI, asInvoker, long paths, common controls v6).
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    let resource = if target_env == "gnu" {
        "assets/vtdrop-gnu.rc"
    } else {
        "assets/vtdrop.rc"
    };
    let _ = embed_resource::compile(resource, embed_resource::NONE);
}

fn main() {
    println!("cargo:rerun-if-changed=assets/caret-mode.rc");
    println!("cargo:rerun-if-changed=assets/caret-mode-icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile_for_everything("assets/caret-mode.rc", embed_resource::NONE)
            .manifest_required()
            .expect("failed to embed the Caret Mode icon");
    }
}

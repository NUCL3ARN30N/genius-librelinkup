#[allow(dead_code)]
#[path = "src/icon.rs"]
mod icon;

fn main() {
    let assets = std::path::Path::new("assets");
    if !assets.join("icon-256.png").exists() || !assets.join("app.ico").exists() {
        icon::write_assets(assets).expect("writing icon assets");
    }
    println!("cargo:rerun-if-changed=src/icon.rs");

    // one catalogue for the UI and the texts made in Rust (gllu_core::i18n), so no context
    let config = slint_build::CompilerConfiguration::new()
        .with_style("material".into())
        .with_bundled_translations("lang")
        .with_default_translation_context(slint_build::DefaultTranslationContext::None);
    println!("cargo:rerun-if-changed=lang");
    slint_build::compile_with_config("ui/app.slint", config).expect("Slint build failed");

    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/app.ico");
        res.set("FileDescription", "Genius LibreLinkUp");
        res.set("ProductName", "Genius LibreLinkUp");
        if let Err(e) = res.compile() {
            println!("cargo:warning=could not embed Windows icon: {e}");
        }
    }
}

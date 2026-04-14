use std::env;
use std::fs;
use std::path::Path;

fn main() {
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap();

    // ── Windows-specific setup ─────────────────────────────────────────────
    if target_os == "windows" {
        if Path::new("assets/icon.ico").exists() {
            println!("cargo:rustc-link-lib=dylib=winmm");
        }
    }

    // ── Ensure output directories exist ──────────────────────────────────
    fs::create_dir_all("target/dist").unwrap();

    // ── Embed font bytes as compile-time constants via custom build script ─
    //   This copies the asset files into the OUT_DIR so they can be
    //   referenced with env!("CARGO_BUILTIN_ASSETS_...") or include_bytes!
    //   from source.  This makes the binary self-contained.
    let out_dir = env::var("OUT_DIR").unwrap();
    let out_path = Path::new(&out_dir);

    // Copy fonts if they exist
    let assets_dir = Path::new("assets");
    let fonts = ["Inter-SemiBold.ttf", "JetBrainsMono-Regular.ttf"];
    let status_dot_png = "status_dot.png";

    // We embed fonts by making them available to include_bytes! in source.
    // The source uses relative paths from the crate root, so we just
    // need the files to be present at compile time.  This block copies
    // them into OUT_DIR so we can also generate a module that exposes them.
    let embed_dir = out_path.join("embedded_assets");
    fs::create_dir_all(&embed_dir).unwrap();

    for font in &fonts {
        let src = assets_dir.join(font);
        if src.exists() {
            fs::copy(&src, embed_dir.join(font)).unwrap();
            println!(
                "cargo:rerun-if-changed={}",
                src.display()
            );
        }
    }

    let dot_src = assets_dir.join(status_dot_png);
    if dot_src.exists() {
        fs::copy(&dot_src, embed_dir.join(status_dot_png)).unwrap();
        println!("cargo:rerun-if-changed={}", dot_src.display());
    }

    // ── Watch the build script and assets directory ─────────────────────────
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/");

    // ── Platform-specific resources ────────────────────────────────────────
    #[cfg(target_os = "windows")]
    {
        if Path::new("assets/icon.ico").exists() {
            // Would be used with the winres crate to embed the icon:
            // println!("cargo:include=assets/icon.ico");
        }
    }
}

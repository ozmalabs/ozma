use std::env;
use std::fs;
use std::path::Path;

fn main() {
    // Enable Windows resource compilation
    if env::var("CARGO_CFG_TARGET_OS").unwrap() == "windows" {
        if Path::new("assets/icon.ico").exists() {
            // This would be used with winres crate for Windows icon embedding
            println!("cargo:rustc-link-lib=dylib=winmm");
        }
    }
    
    // Create necessary directories
    fs::create_dir_all("assets").unwrap();
    fs::create_dir_all("target/dist").unwrap();
    
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/");
}

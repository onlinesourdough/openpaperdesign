//! Embeds the browser bundle into `openpaperdesign-mcp` when it has been built.
//!
//! The bundle comes from `$OPD_WEB_BUNDLE_DIR`, or by default `target/openpaperdesign-web` in
//! this checkout (where `scripts/build-web.sh` writes it). Only a complete bundle is embedded;
//! otherwise the list is empty and the launcher refuses to open the editor ("build the web
//! bundle first") instead of guessing a path at run time.

use std::path::PathBuf;

const FILES: &[&str] = &[
    "index.html",
    "app.js",
    "page.js",
    "session.js",
    "editor.js",
    "panel.js",
    "openpaperdesign_web.js",
    "openpaperdesign_web_bg.wasm",
];

fn main() {
    println!("cargo:rerun-if-env-changed=OPD_WEB_BUNDLE_DIR");
    let dir = std::env::var_os("OPD_WEB_BUNDLE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap())
                .join("../../target/openpaperdesign-web")
        });
    println!("cargo:rerun-if-changed={}", dir.display());
    for f in FILES {
        println!("cargo:rerun-if-changed={}", dir.join(f).display());
    }
    let complete = FILES.iter().all(|f| dir.join(f).is_file());
    let mut code = String::from(
        "/// The browser bundle compiled in (empty when it wasn't built).\n\
         pub static BUNDLE_FILES: &[(&str, &[u8])] = &[\n",
    );
    if complete {
        let dir = dir.canonicalize().unwrap();
        for f in FILES {
            code += &format!(
                "    ({f:?}, include_bytes!({:?})),\n",
                dir.join(f).to_string_lossy()
            );
        }
    }
    code += "];\n";
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("bundle.rs");
    std::fs::write(out, code).unwrap();
}

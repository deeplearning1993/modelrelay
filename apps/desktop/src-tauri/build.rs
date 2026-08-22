//! Tauri build integration embedding the bundled multi-size Windows icon.

use std::{env, path::PathBuf};

fn main() {
    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set"));
    let icon = manifest_dir.join("icons").join("icon.ico");
    println!("cargo:rerun-if-changed={}", icon.display());
    let attributes = tauri_build::Attributes::new()
        .windows_attributes(tauri_build::WindowsAttributes::new().window_icon_path(icon));
    tauri_build::try_build(attributes).expect("build Tauri application resources");
}

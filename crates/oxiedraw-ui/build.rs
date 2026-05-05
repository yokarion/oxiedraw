//! Bundles the hicolor icon tree (`data/icons/hicolor/`) into a GResource so
//! the symbolic tool icons and themed app icon are baked into the binary.
//!
//! Without this the app called `IconTheme::add_search_path("data/icons")`,
//! a path relative to the CWD that doesn't exist next to a shipped binary.
//! Built-in brush PNGs are already embedded via `include_bytes!` in
//! `oxiedraw-core`; this covers the remaining UI/app icons.
//!
//! Inputs: every `*.svg` / `*.png` under `data/icons/hicolor/`.
//! Output: `<OUT_DIR>/oxiedraw-icons.gresource`, pulled in with
//! `include_bytes!`. Uses the system `glib-compile-resources` (shipped with
//! GLib, a hard GTK dependency) rather than a build-time crate, matching the
//! shader build that shells out to `glslc`.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

const RESOURCE_PREFIX: &str = "/io/github/yokarion/OxieDraw/icons";

fn main() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let icons_dir = manifest_dir.join("../../data/icons/hicolor");
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR set by cargo"));

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", icons_dir.display());

    let mut files = Vec::new();
    collect_icons(&icons_dir, &icons_dir, &mut files);
    files.sort();
    if files.is_empty() {
        panic!("no icons found under {}", icons_dir.display());
    }

    let xml_path = out_dir.join("oxiedraw-icons.gresource.xml");
    std::fs::write(&xml_path, build_manifest(&files))
        .unwrap_or_else(|e| panic!("write {}: {e}", xml_path.display()));

    let gresource_path = out_dir.join("oxiedraw-icons.gresource");
    let output = Command::new("glib-compile-resources")
        .arg(format!("--sourcedir={}", icons_dir.display()))
        .arg("--target")
        .arg(&gresource_path)
        .arg(&xml_path)
        .output()
        .unwrap_or_else(|e| {
            panic!(
                "failed to invoke glib-compile-resources: {e}. It ships with \
                 GLib (the `glib2` / `libglib2.0-bin` package), a GTK prerequisite."
            )
        });

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        panic!("glib-compile-resources failed:\n{stderr}");
    }
}

fn collect_icons(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let entries = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read {} ({e})", dir.display()));
    for entry in entries {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            collect_icons(root, &path, out);
        } else if matches!(path.extension().and_then(|e| e.to_str()), Some("svg" | "png")) {
            let rel = path
                .strip_prefix(root)
                .expect("icon under root")
                .to_str()
                .expect("icon path is utf-8")
                .replace('\\', "/");
            out.push(rel);
        }
    }
}

fn build_manifest(files: &[String]) -> String {
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<gresources>\n");
    let _ = writeln!(xml, "  <gresource prefix=\"{RESOURCE_PREFIX}\">");
    for file in files {
        let _ = writeln!(xml, "    <file compressed=\"true\">{file}</file>");
    }
    xml.push_str("  </gresource>\n</gresources>\n");
    xml
}

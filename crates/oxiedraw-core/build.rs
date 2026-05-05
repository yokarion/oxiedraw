//! Compiles GLSL shaders in `shaders/` to SPIR-V using the system `glslc`.
//!
//! Inputs: every `*.vert` / `*.frag` / `*.comp` under `shaders/`.
//! Outputs: `<OUT_DIR>/<name>.<stage>.spv`. The Rust code then pulls them
//! in with `include_bytes!(concat!(env!("OUT_DIR"), "/foo.frag.spv"))`.
//!
//! Why system glslc and not the `shaderc` crate? `shaderc` will fall back
//! to compiling libshaderc from source on a fresh machine, which can take
//! ten minutes. `glslc` is in the `shaderc` package on Arch and `vulkan-tools`
//! on Debian/Fedora and is a stock prerequisite for any Vulkan dev box.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let shaders_dir = manifest_dir.join("shaders");
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR set by cargo"));

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", shaders_dir.display());

    if !shaders_dir.exists() {
        // No shaders yet - empty build, nothing to compile.
        return;
    }

    let entries = std::fs::read_dir(&shaders_dir)
        .unwrap_or_else(|e| panic!("read shaders/ ({}): {e}", shaders_dir.display()));

    for entry in entries {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        let Some(stage) = shader_stage(&path) else {
            continue;
        };
        compile(&path, stage, &out_dir);
    }
}

fn shader_stage(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()? {
        "vert" => Some("vertex"),
        "frag" => Some("fragment"),
        "comp" => Some("compute"),
        _ => None,
    }
}

fn compile(input: &Path, stage: &str, out_dir: &Path) {
    println!("cargo:rerun-if-changed={}", input.display());

    let file_name = input
        .file_name()
        .expect("input has filename")
        .to_str()
        .expect("filename is utf-8");
    let out_path = out_dir.join(format!("{file_name}.spv"));

    let output = Command::new("glslc")
        .arg(format!("-fshader-stage={stage}"))
        .arg("-O")
        .arg("--target-env=vulkan1.3")
        .arg("-o")
        .arg(&out_path)
        .arg(input)
        .output()
        .unwrap_or_else(|e| {
            panic!(
                "failed to invoke glslc: {e}. Install the `shaderc` (Arch) \
                 or `vulkan-tools` / `glslang-tools` (Debian/Fedora) package."
            )
        });

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        panic!("glslc failed for {}:\n{stderr}", input.display(),);
    }
}

//! sherpa-onnx のインポートライブラリをリンクし、実行用 DLL を
//! ビルド成果物の隣へコピーする。

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        println!("cargo:warning=mkvoicejp は Windows 専用です (sherpa-onnx DLL をリンクしません)");
        return;
    }

    let manifest = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR がありません"),
    );
    let root = manifest.join("sherpa-onnx-v1.13.8-win-x64-shared-MD-Release");
    let lib_dir = root.join("lib");
    let import_lib = lib_dir.join("sherpa-onnx-c-api.lib");
    if !import_lib.is_file() {
        panic!(
            "sherpa-onnx のインポートライブラリが見つかりません: {}\n\
             sherpa-onnx-v1.13.8-win-x64-shared-MD-Release を配置してください。",
            import_lib.display()
        );
    }

    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=dylib=sherpa-onnx-c-api");
    println!("cargo:rerun-if-changed={}", import_lib.display());

    // 実行時に exe と同じディレクトリから DLL を解決できるようコピーする。
    let Ok(out_dir) = std::env::var("OUT_DIR") else {
        return;
    };
    let mut profile_dir = PathBuf::from(out_dir);
    for _ in 0..3 {
        profile_dir.pop();
    }
    let bin_dir = root.join("lib");
    for name in [
        "sherpa-onnx-c-api.dll",
        "onnxruntime.dll",
        "onnxruntime_providers_shared.dll",
    ] {
        let source = bin_dir.join(name);
        if !source.is_file() {
            println!("cargo:warning={} が見つかりません", source.display());
            continue;
        }
        for target_dir in [profile_dir.clone(), profile_dir.join("deps")] {
            if std::fs::create_dir_all(&target_dir).is_err() {
                continue;
            }
            let target = target_dir.join(name);
            if let Err(exc) = std::fs::copy(&source, &target) {
                println!(
                    "cargo:warning={} を {} へコピーできません: {exc}",
                    source.display(),
                    target.display()
                );
            }
        }
    }
    println!("cargo:rerun-if-changed={}", bin_dir.display());
}

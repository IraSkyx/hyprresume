use std::process::Command;

fn main() {
    let hyprland = pkg_config::Config::new()
        .probe("hyprland")
        .expect("hyprland pkg-config not found");

    let out = std::env::var("OUT_DIR").unwrap();

    // build include args
    let mut includes = Vec::new();
    if let Ok(src) = std::env::var("HYPRLAND_SOURCE") {
        let link = format!("{out}/include/hyprland");
        drop(std::fs::create_dir_all(format!("{out}/include")));
        drop(std::fs::remove_file(&link));
        std::os::unix::fs::symlink(&src, &link).ok();
        includes.push(format!("-I{out}/include"));
    }
    for path in &hyprland.include_paths {
        includes.push(format!("-I{}", path.display()));
    }

    // compile shim.cpp → shim.o
    let obj = format!("{out}/shim.o");
    let status = Command::new("c++")
        .args(["-std=c++26", "-fPIC", "-O2", "-w", "-c", "shim.cpp", "-o", &obj])
        .args(&includes)
        .status()
        .expect("failed to run c++");
    assert!(status.success(), "shim.cpp compilation failed");

    // create static archive
    let ar = format!("{out}/libshim.a");
    drop(std::fs::remove_file(&ar));
    let status = Command::new("ar")
        .args(["rcs", &ar, &obj])
        .status()
        .expect("failed to run ar");
    assert!(status.success(), "ar failed");

    // link with --whole-archive so all C++ symbols are exported
    println!("cargo:rustc-cdylib-link-arg=-Wl,--whole-archive");
    println!("cargo:rustc-cdylib-link-arg={ar}");
    println!("cargo:rustc-cdylib-link-arg=-Wl,--no-whole-archive");

    println!("cargo:rustc-link-lib=stdc++");
    for lib in &hyprland.libs {
        println!("cargo:rustc-link-lib={lib}");
    }

    // override the default Rust version script to export C++ plugin symbols
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:rustc-cdylib-link-arg=-Wl,--version-script={manifest}/exports.map");

    println!("cargo:rerun-if-changed=shim.cpp");
    println!("cargo:rerun-if-changed=exports.map");
    println!("cargo:rerun-if-env-changed=HYPRLAND_SOURCE");
}

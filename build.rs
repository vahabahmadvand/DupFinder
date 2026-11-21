use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let icon_path = manifest_dir.join("assets").join("icon.ico");
    if !icon_path.exists() {
        return;
    }

    let Some(rc_exe) = find_rc_exe() else {
        println!("cargo:warning=rc.exe not found; Windows .exe icon will not be embedded");
        return;
    };

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let rc_file = out_dir.join("dupfinder.rc");
    let res_file = out_dir.join("dupfinder.res");

    // Forward slashes work cleanly in Windows RC scripts without backslash escaping issues
    let icon_rc_path = icon_path.to_string_lossy().replace('\\', "/");
    let rc_content = format!("1 ICON \"{icon_rc_path}\"\n");
    if fs::write(&rc_file, rc_content).is_err() {
        return;
    }

    let status = Command::new(&rc_exe)
        .arg("/nologo")
        .arg("/fo")
        .arg(&res_file)
        .arg(&rc_file)
        .status();

    match status {
        Ok(s) if s.success() && res_file.exists() => {
            println!("cargo:rustc-link-arg-bins={}", res_file.display());
        }
        _ => {
            println!("cargo:warning=rc.exe failed to compile icon resource");
        }
    }
}

fn find_rc_exe() -> Option<PathBuf> {
    // 1. Check PATH first
    if let Ok(path_var) = env::var("PATH") {
        for dir in env::split_paths(&path_var) {
            let candidate = dir.join("rc.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    // 2. Search Windows Kits 10 SDK (pick latest installed version)
    let arch = match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("x86") => "x86",
        Ok("aarch64") => "arm64",
        _ => "x64",
    };

    let kits_bin = Path::new(r"C:\Program Files (x86)\Windows Kits\10\bin");
    let mut versions: Vec<PathBuf> = fs::read_dir(kits_bin)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    versions.sort();
    versions.reverse();

    for ver_dir in versions {
        let candidate = ver_dir.join(arch).join("rc.exe");
        if candidate.is_file() {
            return Some(candidate);
        }
    }

    None
}

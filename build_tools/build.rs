use std::{env, fs, path::PathBuf, process::Command};

use wolfram_app_discovery::WolframApp;

fn main() {
    println!("cargo:rerun-if-env-changed=WOLFRAM_KERNEL");

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR is set by Cargo"));
    let output_path = out_dir.join("builtin_symbols.tsv");

    println!("cargo:rerun-if-changed=build_tools/wl/builtin_symbols.wl");
    println!("cargo:rerun-if-changed=build_tools/wl/query_to_output_form.wl");
    let code = include_str!("wl/builtin_symbols.wl");

    let output = Command::new(kernel_path())
        .arg("-noprompt")
        .arg("-run")
        .arg(include_str!("wl/query_to_output_form.wl").replace("__CODE__", code))
        .output();

    match output {
        Ok(output) if output.status.success() => {
            fs::write(&output_path, output.stdout).expect("failed to write builtin symbol table");
        }
        Ok(output) => {
            println!(
                "cargo:warning=failed to build builtin symbol table with WolframKernel: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
            fs::write(&output_path, "").expect("failed to write empty builtin symbol table");
        }
        Err(err) => {
            println!(
                "cargo:warning=failed to launch WolframKernel for builtin symbol table: {err}"
            );
            fs::write(&output_path, "").expect("failed to write empty builtin symbol table");
        }
    }
}

fn kernel_path() -> PathBuf {
    if let Some(path) = env::var_os("WOLFRAM_KERNEL") {
        return PathBuf::from(path);
    }

    if let Some(path) = wolframscript_showkernels_kernel_path() {
        return path;
    }

    if let Ok(app) = WolframApp::try_default() {
        let install_dir = app.installation_directory();
        if let Some(path) = native_kernel_path(&install_dir) {
            if path.exists() {
                return path;
            }
        }

        let path = install_dir.join("Executables").join(kernel_binary_name());
        if path.exists() {
            return path;
        }
    }

    PathBuf::from(kernel_binary_name())
}

fn wolframscript_showkernels_kernel_path() -> Option<PathBuf> {
    let output = Command::new("wolframscript")
        .arg("-showkernels")
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8(output.stdout).ok()?;
    stdout.lines().map(str::trim).find_map(|line| {
        let path = PathBuf::from(line);
        (path
            .file_name()
            .is_some_and(|name| name == kernel_binary_name())
            && path.exists())
        .then_some(path)
    })
}

fn native_kernel_path(install_dir: &std::path::Path) -> Option<PathBuf> {
    let platform = if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "Linux-x86-64"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "Linux-ARM64"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "MacOSX-x86-64"
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "MacOSX-ARM64"
    } else if cfg!(all(windows, target_arch = "x86_64")) {
        "Windows-x86-64"
    } else {
        return None;
    };

    Some(
        install_dir
            .join("SystemFiles")
            .join("Kernel")
            .join("Binaries")
            .join(platform)
            .join(kernel_binary_name()),
    )
}

fn kernel_binary_name() -> &'static str {
    if cfg!(windows) {
        "WolframKernel.exe"
    } else {
        "WolframKernel"
    }
}

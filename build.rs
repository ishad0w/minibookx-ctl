// SPDX-License-Identifier: Apache-2.0
use std::{env, fs, path::Path, process::Command};

fn license_files(base: &Path, directory: &Path, prefix: &str, files: &mut Vec<(String, String)>) {
    println!("cargo::rerun-if-changed={}", directory.display());
    let mut entries: Vec<_> = fs::read_dir(directory)
        .expect("read bundled license directory")
        .map(|entry| entry.expect("read bundled license entry").path())
        .collect();
    entries.sort();
    for path in entries {
        let metadata = fs::symlink_metadata(&path).expect("stat bundled license");
        assert!(
            !metadata.is_symlink(),
            "license input must not be a symlink"
        );
        if metadata.is_dir() {
            license_files(base, &path, prefix, files);
        } else {
            assert!(metadata.is_file(), "license input must be a regular file");
            let relative = path
                .strip_prefix(base)
                .expect("license path below its root");
            let relative = relative.to_str().expect("license destination UTF-8");
            files.push((
                format!("{prefix}{relative}"),
                path.to_str().expect("license input UTF-8").into(),
            ));
            println!("cargo::rerun-if-changed={}", path.display());
        }
    }
}

fn bundle_licenses(root: &Path) {
    let compiler = env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let output = Command::new(compiler)
        .args(["--print", "sysroot"])
        .output()
        .expect("query Rust sysroot for its runtime license notices");
    assert!(output.status.success(), "query Rust sysroot");
    let sysroot = String::from_utf8(output.stdout).expect("Rust sysroot UTF-8");
    let rust_docs = Path::new(sysroot.trim()).join("share/doc/rust");
    let mut files = Vec::new();
    let licenses = root.join("packaging/licenses");
    license_files(&licenses, &licenses, "", &mut files);
    // Source distributions already contain notices from their recorded compiler.
    // The executable always ships the notices for the compiler used for this build.
    files.retain(|(name, _)| !name.starts_with("rust-runtime/"));
    let runtime = rust_docs.join("licenses");
    let copyright = rust_docs.join("COPYRIGHT-library.html");
    assert!(
        runtime.is_dir() && copyright.is_file(),
        "Rust runtime license notices are missing; install rust-docs for this compiler with `rustup component add rust-docs`"
    );
    license_files(&rust_docs, &runtime, "rust-runtime/", &mut files);
    println!("cargo::rerun-if-changed={}", copyright.display());
    files.push((
        "rust-runtime/COPYRIGHT-library.html".into(),
        copyright.to_str().expect("copyright path UTF-8").into(),
    ));
    files.sort();
    assert!(
        files.windows(2).all(|pair| pair[0].0 != pair[1].0),
        "duplicate bundled license destination"
    );
    let mut source = String::from("pub const BUNDLED_LICENSES: &[(&str, &[u8])] = &[\n");
    for (destination, input) in files {
        source.push_str(&format!(
            "    ({destination:?}, include_bytes!({input:?})),\n"
        ));
    }
    source.push_str("];\n");
    let output = env::var_os("OUT_DIR").expect("Cargo build output directory");
    fs::write(Path::new(&output).join("license_manifest.rs"), source)
        .expect("write bundled license manifest");
}

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8(output.stdout).ok())
        .flatten()
        .map(|text| text.trim().to_owned())
}

fn valid_commit(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    bundle_licenses(root);
    let target = env::var("TARGET").expect("Cargo target");
    let compiler = Command::new(env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
        .arg("-V")
        .output()
        .expect("query Cargo's Rust compiler");
    assert!(compiler.status.success(), "query Rust compiler version");
    let compiler = String::from_utf8(compiler.stdout).expect("Rust version UTF-8");
    let compiler = compiler.trim();
    assert!(compiler.is_ascii() && !compiler.contains(['\n', '\r']) && compiler.len() < 1024);
    println!("cargo::rustc-env=MINIBOOKX_BUILD_RUSTC={compiler}");
    println!("cargo::rerun-if-env-changed=RUSTC");
    let static_dbus = if target == "x86_64-unknown-linux-musl" {
        let prefix = root.join("target/native/libdbus-musl");
        let library = prefix.join("lib/libdbus-1.a");
        for file in [&library, &prefix.join("build-info.json")] {
            println!("cargo::rerun-if-changed={}", file.display());
        }
        // Watch the actual linked archive, not just Cargo's Rust inputs. A C
        // rebuild at the same Git HEAD must relink the final executable too.
        assert_eq!(env::var("DBUS_1_STATIC").as_deref(), Ok("1"));
        assert_eq!(
            env::var_os("PKG_CONFIG_LIBDIR").map(std::path::PathBuf::from),
            Some(prefix.join("lib/pkgconfig")),
            "use scripts/build-static.sh for the reviewed static C input"
        );
        let output = Command::new("sha256sum")
            .arg("--")
            .arg(&library)
            .output()
            .expect("sha256sum is required for static build provenance");
        assert!(
            output.status.success(),
            "build the pinned libdbus archive first"
        );
        let digest = String::from_utf8(output.stdout)
            .expect("sha256sum output")
            .split_whitespace()
            .next()
            .expect("archive SHA-256")
            .to_owned();
        assert!(digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()));
        digest
    } else {
        String::new()
    };
    println!("cargo::rustc-env=MINIBOOKX_LIBDBUS_SHA256={static_dbus}");
    for name in ["DBUS_1_STATIC", "PKG_CONFIG_LIBDIR"] {
        println!("cargo::rerun-if-env-changed={name}");
    }
    for name in ["MINIBOOKX_BUILD_COMMIT", "MINIBOOKX_BUILD_DIRTY"] {
        println!("cargo::rerun-if-env-changed={name}");
    }
    for name in [
        "build.rs",
        "Cargo.toml",
        "Cargo.lock",
        "BUILD-COMMIT",
        "src",
        "config",
        "packaging",
        "scripts",
        "docs",
        "contrib",
        "tests",
        "README.md",
        "AGENTS.md",
        "MAP.md",
        "SECURITY.md",
        ".github",
        ".gitignore",
        ".gitattributes",
        "rust-toolchain.toml",
        "rustfmt.toml",
        "clippy.toml",
        "install.sh",
        "LICENSE",
        "NOTICE",
        "THIRD_PARTY.md",
        "third-party",
        "Makefile",
        ".editorconfig",
    ] {
        println!("cargo::rerun-if-changed={name}");
    }
    // A source archive may be unpacked inside an unrelated Git checkout.
    let own_git = git(root, &["rev-parse", "--show-toplevel"])
        .and_then(|top| Path::new(&top).canonicalize().ok())
        == root.canonicalize().ok();
    if own_git && let Some(directory) = git(root, &["rev-parse", "--absolute-git-dir"]) {
        let directory = Path::new(&directory);
        for name in ["HEAD", "packed-refs"] {
            println!("cargo::rerun-if-changed={}", directory.join(name).display());
        }
        if let Ok(head) = fs::read_to_string(directory.join("HEAD"))
            && let Some(reference) = head.trim().strip_prefix("ref: ")
        {
            println!(
                "cargo::rerun-if-changed={}",
                directory.join(reference).display()
            );
        }
    }
    let from_git = own_git.then(|| git(root, &["rev-parse", "HEAD"])).flatten();
    let explicit = env::var("MINIBOOKX_BUILD_COMMIT").ok();
    if let Some(value) = &explicit {
        assert!(
            valid_commit(value),
            "MINIBOOKX_BUILD_COMMIT must be a full Git SHA"
        );
        if let Some(actual) = &from_git {
            assert_eq!(
                value.to_ascii_lowercase(),
                *actual,
                "build SHA differs from Git HEAD"
            );
        }
    }
    let commit = explicit
        .or(from_git)
        .or_else(|| {
            fs::read_to_string(root.join("BUILD-COMMIT"))
                .ok()
                .map(|s| s.trim().to_owned())
        })
        .filter(|value| valid_commit(value))
        .map(|value| value.to_ascii_lowercase())
        .unwrap_or_else(|| "unknown".into());
    let git_dirty = own_git
        && git(root, &["status", "--porcelain", "--untracked-files=normal"])
            .is_none_or(|status| !status.is_empty());
    let dirty = match env::var("MINIBOOKX_BUILD_DIRTY").as_deref() {
        Ok("true") => true,
        Ok("false") => {
            assert!(!git_dirty, "cannot label a dirty checkout as a clean build");
            false
        }
        Ok(_) => panic!("MINIBOOKX_BUILD_DIRTY must be true or false"),
        Err(_) => git_dirty,
    };
    let short = if commit == "unknown" {
        "unknown"
    } else {
        &commit[..12]
    };
    println!("cargo::rustc-env=MINIBOOKX_BUILD_COMMIT={commit}");
    println!("cargo::rustc-env=MINIBOOKX_BUILD_SHORT={short}");
    println!(
        "cargo::rustc-env=MINIBOOKX_BUILD_DIRTY_SUFFIX={}",
        if dirty { ".dirty" } else { "" }
    );
    println!("cargo::rustc-env=MINIBOOKX_BUILD_TARGET={}", target);
}

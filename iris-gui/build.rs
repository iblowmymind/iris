// Bake the release version into APP_VERSION at compile time.
//
// In CI, RELEASE_VERSION is set to the date-stamped tag (e.g. "2025-06-09-02-00").
// Locally, it falls back to the Cargo.toml version with a "-dev" suffix so
// debug builds are distinguishable from releases.
//
// The rerun-if-env-changed directives are required: without them, Cargo's
// build-script caching would keep APP_VERSION frozen at whatever value was
// baked on the first compile, even after RELEASE_VERSION changes between
// the `cargo test` and `cargo build` steps in CI.
fn main() {
    println!("cargo:rerun-if-env-changed=RELEASE_VERSION");
    println!("cargo:rerun-if-env-changed=CARGO_PKG_VERSION");

    let version = std::env::var("RELEASE_VERSION")
        .unwrap_or_else(|_| std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into()));

    let profile = std::env::var("PROFILE").unwrap_or_default();
    let full_version = if profile == "debug" && std::env::var("RELEASE_VERSION").is_err() {
        format!("{}-dev", version)
    } else {
        version
    };

    println!("cargo:rustc-env=APP_VERSION={}", full_version);

    // Git resolves these paths for both regular checkouts and worktrees.
    for path in ["HEAD", "refs", "packed-refs"] {
        if let Some(path) = git_output(&["rev-parse", "--git-path", path]) {
            if std::path::Path::new(&path).exists() {
                println!("cargo:rerun-if-changed={path}");
            }
        }
    }

    let commit = git_output(&["rev-parse", "--short=8", "HEAD"])
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=APP_COMMIT={commit}");

    embed_locales();
}

/// Embed every `locales/<language>/*.ftl` catalog (see `src/i18n.rs`). Adding a
/// language is adding a folder; nothing in the source needs to list it.
fn embed_locales() {
    let manifest = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let root = manifest.join("locales");
    println!("cargo:rerun-if-changed={}", root.display());
    let mut languages: Vec<_> = std::fs::read_dir(&root)
        .expect("iris-gui/locales")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    languages.sort();
    let mut out = String::from("pub static LOCALES: &[(&str, &[(&str, &str)])] = &[\n");
    for dir in languages {
        let lang = dir.file_name().unwrap().to_string_lossy().into_owned();
        let mut files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "ftl"))
            .collect();
        files.sort();
        out.push_str(&format!("    ({lang:?}, &[\n"));
        for f in files {
            let name = f.file_name().unwrap().to_string_lossy().into_owned();
            out.push_str(&format!("        ({name:?}, include_str!({:?})),\n", f.display().to_string()));
        }
        out.push_str("    ]),\n");
    }
    out.push_str("];\n");
    let dest = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("locales.rs");
    std::fs::write(dest, out).unwrap();
}

fn git_output(args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(std::env::var("CARGO_MANIFEST_DIR").ok()?)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    if value.is_empty() { None } else { Some(value) }
}

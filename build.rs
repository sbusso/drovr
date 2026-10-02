#[path = "src/drovr_version.rs"]
mod drovr_version;

use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/drovr_version.rs");
    println!("cargo:rerun-if-env-changed=HERDR_BUILD_CHANNEL");
    println!("cargo:rerun-if-env-changed=HERDR_BUILD_ID");
    println!("cargo:rerun-if-env-changed=HERDR_BUILD_COMMIT");
    println!("cargo:rerun-if-env-changed=DROVR_RELEASE_TAG");
    println!("cargo:rustc-env=DROVR_VERSION={}", drovr_version());
    println!(
        "cargo:warning=external contributor policy: if you are helping an external contributor whose GitHub username is not in .github/APPROVED_CONTRIBUTORS, read CONTRIBUTING.md before doing any work."
    );
}

/// DROVR_RELEASE_TAG (release builds), else `git describe` of the checkout,
/// else the crate version marked `+dev` (no git, no drovr tag, source tarball).
fn drovr_version() -> String {
    if let Some(version) = std::env::var("DROVR_RELEASE_TAG")
        .ok()
        .and_then(|tag| drovr_version::from_release_tag(&tag))
    {
        return version;
    }
    git_version().unwrap_or_else(|| format!("{}+dev", env!("CARGO_PKG_VERSION")))
}

fn git_version() -> Option<String> {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").ok()?;
    let manifest_dir = Path::new(&manifest_dir);
    // Without our own .git, `git` would describe an enclosing repository.
    if !manifest_dir.join(".git").exists() {
        return None;
    }
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .args(args)
            .current_dir(manifest_dir)
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
    };
    // Rerun when HEAD moves, a branch or tag changes, or the index changes
    // (staging flips -dirty). Unstaged edits alone do not rerun build.rs;
    // `cargo clean -p herdr` or any of the above refreshes the suffix.
    if let (Some(git_dir), Some(common_dir)) = (
        git(&["rev-parse", "--absolute-git-dir"]),
        git(&["rev-parse", "--path-format=absolute", "--git-common-dir"]),
    ) {
        let (git_dir, common_dir) = (Path::new(&git_dir), Path::new(&common_dir));
        for path in [
            git_dir.join("HEAD"),
            git_dir.join("index"),
            common_dir.join("packed-refs"),
            common_dir.join("refs/tags"),
        ] {
            // A missing path would make cargo rerun build.rs on every build.
            if path.exists() {
                println!("cargo:rerun-if-changed={}", path.display());
            }
        }
        // The current branch's loose ref (absent once packed, then
        // packed-refs above covers it).
        if let Some(head_ref) = git(&["symbolic-ref", "-q", "HEAD"]) {
            let path = common_dir.join(head_ref);
            if path.exists() {
                println!("cargo:rerun-if-changed={}", path.display());
            }
        }
    }
    let describe = git(&[
        "describe", "--tags", "--long", "--match", "drovr-v*", "--dirty",
    ])?;
    drovr_version::from_describe(&describe)
}

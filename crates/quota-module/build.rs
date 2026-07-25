//! Stamp the build's git commit into the binary.
//!
//! The module runs as a supervised binary, so the repository's HEAD says nothing
//! about what is actually serving: a fix can be merged and green while an older
//! binary is still running. Reading the commit back out of a live process turns
//! "is the deployed build current?" into an exact question answerable remotely,
//! rather than an inference from a file's modification time.
//!
//! Resolved from `.git` directly rather than by running `git`, so the build does
//! not depend on a git binary being present. An unavailable or unreadable
//! repository yields "unknown" rather than failing the build, because a stamp is
//! diagnostic and must never be the reason a release cannot be built.

use std::path::{Path, PathBuf};

fn main() {
    let git_dir = locate_git_dir();
    let commit = git_dir.as_deref().and_then(head_commit);

    // Rebuild when HEAD moves, so the stamp cannot go stale within a worktree.
    if let Some(dir) = git_dir.as_deref() {
        println!("cargo:rerun-if-changed={}/HEAD", dir.display());
        if let Some(path) = head_ref_path(dir) {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!(
        "cargo:rustc-env=CK_QUOTA_BUILD_COMMIT={}",
        commit.as_deref().unwrap_or("unknown")
    );
}

/// Walk up from the crate directory to the repository root holding `.git`.
///
/// `.git` is a directory in a normal checkout and a FILE containing a `gitdir:`
/// pointer inside a worktree, which is how the delegated worker checkouts are
/// laid out — both are resolved here so a worktree build stamps correctly.
fn locate_git_dir() -> Option<PathBuf> {
    let mut dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").ok()?);
    loop {
        let candidate = dir.join(".git");
        if candidate.is_dir() {
            return Some(candidate);
        }
        if candidate.is_file() {
            let contents = std::fs::read_to_string(&candidate).ok()?;
            let target = contents.strip_prefix("gitdir:")?.trim();
            return Some(dir.join(target));
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// The path `HEAD` points at, when it is a symbolic ref rather than detached.
fn head_ref_path(git_dir: &Path) -> Option<PathBuf> {
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let reference = head.trim().strip_prefix("ref: ")?.trim().to_string();
    // A worktree's HEAD ref lives in the MAIN repository's directory, which is the
    // parent of `worktrees/<name>`; fall back to it when the local path is absent.
    let local = git_dir.join(&reference);
    if local.exists() {
        return Some(local);
    }
    let common = git_dir.parent()?.parent()?.join(&reference);
    common.exists().then_some(common)
}

/// The commit `HEAD` resolves to: the ref's contents, or `HEAD` itself when
/// detached. Packed refs are not consulted, so a build from a fully packed
/// checkout stamps "unknown" rather than a wrong value.
fn head_commit(git_dir: &Path) -> Option<String> {
    match head_ref_path(git_dir) {
        Some(path) => read_sha(&path),
        None => {
            let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
            let head = head.trim();
            (!head.starts_with("ref:")).then(|| short(head))
        }
    }
}

fn read_sha(path: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let raw = raw.trim();
    (!raw.is_empty()).then(|| short(raw))
}

fn short(sha: &str) -> String {
    sha.chars().take(12).collect()
}

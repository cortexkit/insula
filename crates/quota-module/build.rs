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
//!
//! The stamp is `HEAD`, which is the commit the build STARTED FROM rather than a
//! description of what was compiled: a build from a dirty tree carries the clean
//! sha verbatim. It therefore answers "is this build missing commits", and
//! cannot answer "does it contain anything extra". Checking the tree is clean
//! before building a release binary is what closes that half, and it is a
//! deliberate division — hashing the tree would make every uncommitted edit
//! during development produce a new stamp and a full rebuild.

use std::path::{Path, PathBuf};

/// Overrides the resolved commit, so two builds can be made byte-comparable.
///
/// The stamp is compiled in, so a binary built from one commit differs from one
/// built at another even when no runtime code changed between them -- which
/// makes comparing their hashes useless for the question people actually ask
/// before a deploy: *does this change affect the running binary at all?*
///
/// Pinning both builds to the same stamp makes them comparable. Build the two
/// commits from the same directory with the same override and compare hashes.
///
/// **Only equality is conclusive.** Identical bytes prove the two commits
/// produce the same binary, so a change that touches only tests needs no
/// deploy -- which is the case worth having a fast, exact answer for, since it
/// is the usual one. A difference proves nothing on its own: panic messages
/// embed their own file and line, so inserting a comment above a function in a
/// runtime file changes the binary without changing behaviour. Fall back to
/// reading the diff in that case.
///
/// Two conditions, both easy to get wrong: the builds must run from the SAME
/// directory, because absolute paths are embedded and a worktree elsewhere
/// produces different bytes for identical source; and both must use this
/// override, since a commit predating it ignores the variable and stamps its
/// own sha, making the stamp itself the difference being measured.
const STAMP_OVERRIDE: &str = "CK_QUOTA_BUILD_COMMIT_OVERRIDE";

fn main() {
    let git_dir = locate_git_dir();

    // Rebuild when HEAD moves, so the stamp cannot go stale within a worktree.
    if let Some(dir) = git_dir.as_deref() {
        println!("cargo:rerun-if-changed={}/HEAD", dir.display());
        if let Some(path) = head_ref_path(dir) {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed={STAMP_OVERRIDE}");

    let commit = match std::env::var(STAMP_OVERRIDE) {
        // Only a well-formed stamp is honoured, so a stray or malformed value
        // cannot put an arbitrary string where a commit is expected.
        Ok(value) if is_stamp(&value) => Some(value),
        _ => git_dir.as_deref().and_then(head_commit),
    };

    println!(
        "cargo:rustc-env=CK_QUOTA_BUILD_COMMIT={}",
        commit.as_deref().unwrap_or("unknown")
    );
}

/// Whether a string has the shape this build stamps: short hex, as [`short`]
/// produces.
fn is_stamp(raw: &str) -> bool {
    raw.len() == 12 && raw.chars().all(|c| c.is_ascii_hexdigit())
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

/// The commit `HEAD` resolves to: the ref's file, the packed-refs table, or
/// `HEAD` itself when detached.
///
/// Packing is not an unusual state to find a repository in -- `git gc` packs
/// refs as part of routine maintenance, and it runs automatically. Without the
/// packed table a maintained checkout stamps "unknown", which fails safe but
/// costs the deploy check its only instrument: it can no longer answer whether
/// the running binary is the one that was built.
fn head_commit(git_dir: &Path) -> Option<String> {
    if let Some(sha) = head_ref_path(git_dir).as_deref().and_then(read_sha) {
        return sha_if_valid(&sha);
    }
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    match head.strip_prefix("ref: ") {
        Some(reference) => packed_ref(git_dir, reference.trim()),
        None => sha_if_valid(head),
    }
}

/// Look `reference` up in the packed-refs table.
///
/// Its lines are `<sha> <refname>`, with `#` comments and `^<sha>` peel lines
/// for annotated tags -- neither of which can match, since a peel line has no
/// second field and a comment's first field is not a ref name.
///
/// A worktree keeps its own `HEAD` but shares the main repository's refs, so the
/// table is read from the common directory when the local one has no entry.
fn packed_ref(git_dir: &Path, reference: &str) -> Option<String> {
    let mut candidates = vec![git_dir.join("packed-refs")];
    if let Some(common) = git_dir.parent().and_then(Path::parent) {
        candidates.push(common.join("packed-refs"));
    }
    for path in candidates {
        let Ok(table) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in table.lines() {
            let Some((sha, name)) = line.split_once(' ') else {
                continue;
            };
            if name.trim() == reference {
                return sha_if_valid(sha);
            }
        }
    }
    None
}

/// Accept only a full hex object id, so a malformed line cannot become a stamp
/// that looks like a commit.
fn sha_if_valid(raw: &str) -> Option<String> {
    let raw = raw.trim();
    (raw.len() >= 40 && raw.chars().all(|c| c.is_ascii_hexdigit())).then(|| short(raw))
}

fn read_sha(path: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let raw = raw.trim().to_string();
    (!raw.is_empty()).then_some(raw)
}

fn short(sha: &str) -> String {
    sha.chars().take(12).collect()
}

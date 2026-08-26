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
    // Re-run when the lockfile moves, or the stamped version outlives the bump.
    println!("cargo:rerun-if-changed=../../Cargo.lock");

    // Provenance stamps, kept SEPARATE from CK_QUOTA_BUILD_COMMIT above rather
    // than replacing it. The health stamp answers "is this build missing
    // commits" and is documented as carrying a clean sha from a dirty tree;
    // these answer an identity question and must not. One value cannot mean both
    // things, and reusing it read as obviously correct until someone checked.
    let worktree = locate_worktree_root();
    let clean = worktree.as_deref().and_then(tree_clean);
    println!(
        "cargo:rustc-env=CK_QUOTA_PROVENANCE_SHA={}",
        // head_commit takes the GIT DIR (which for a worktree is not under the
        // worktree root at all); tree_clean and lock_digest take the WORKTREE
        // ROOT. Two different paths, and passing one to the other compiles fine
        // and yields None on every worktree build.
        match (clean, locate_git_dir().as_deref().and_then(head_commit)) {
            // The ONLY case that may state a commit: we looked, and it was clean.
            (Some(true), Some(sha)) => short(&sha),
            // Dirty, or undeterminable, or no HEAD. Empty string, which the
            // consumer maps to None -- a sentinel must never render as a value.
            _ => String::new(),
        }
    );
    println!(
        "cargo:rustc-env=CK_QUOTA_LOCK_DIGEST={}",
        worktree
            .as_deref()
            .and_then(lock_digest)
            .unwrap_or_default()
    );
    let manifest_dir = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    println!(
        "cargo:rustc-env=CK_QUOTA_WIRE_CRATE_VERSION={}",
        wire_crate_version(&manifest_dir).unwrap_or_default()
    );
}

/// Whether a string has the shape this build stamps: short hex, as [`short`]
/// produces.
/// The directory CONTAINING `.git`, which is what `git status` must run from.
///
/// NOT `locate_git_dir().parent()`. That is right for a normal checkout and wrong
/// for a worktree, where `.git` is a FILE pointing at
/// `<main>/.git/worktrees/<name>` — whose parent is `<main>/.git/worktrees`.
/// Running `git status` there exits 128 ("this operation must be run in a work
/// tree"), and since a non-zero exit is treated as undeterminable, the clean
/// check would return None on EVERY worktree build. The emit arm would be
/// structurally unreachable and its absence would look exactly like a correctly
/// detected dirty tree. Reported on insula#12 by someone who hit it.
fn locate_worktree_root() -> Option<PathBuf> {
    let mut dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").ok()?);
    loop {
        if dir.join(".git").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Whether the working tree had no uncommitted changes.
///
/// `Some(true)` clean, `Some(false)` dirty, `None` UNDETERMINABLE — git missing,
/// non-zero exit, or unreadable output. None and false are deliberately distinct
/// at the type level even though both suppress the stamp, because "we looked and
/// it was dirty" and "we could not look" are different facts and collapsing them
/// is the error this repo keeps finding elsewhere.
///
/// DEGRADES ALONE. The HEAD walk reads `.git` directly and needs no git binary;
/// only this check does. A source-tarball build with no git therefore still
/// answers "is this missing commits" via the health stamp, and simply declines
/// to make the stronger provenance claim.
fn tree_clean(worktree_root: &Path) -> Option<bool> {
    let out = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(worktree_root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8(out.stdout).ok()?.trim().is_empty())
}

/// Hash the resolved lockfile, so a build that cannot claim a commit can still
/// be told apart from a different build.
///
/// A BUILD-TIME READ, which is what the contract requires. The prohibited
/// version is reading Cargo.lock when the manifest is CONSTRUCTED: that
/// describes the source tree sitting beside the running binary rather than the
/// binary itself, and the two diverge the moment anyone pulls.
///
/// Deliberately not gated on tree cleanliness. It is a change-detector, not an
/// identity claim — it answers "are these two builds the same dependencies",
/// which stays true and useful on a dirty tree where the sha does not.
fn lock_digest(worktree_root: &Path) -> Option<String> {
    use std::hash::{Hash, Hasher};
    let text = std::fs::read(worktree_root.join("Cargo.lock")).ok()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    Some(format!("{:016x}", hasher.finish()))
}

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

/// Resolve the wire crate's version from the tracked lockfile.
///
/// READ RATHER THAN HARDCODED, because a literal here is a second place the
/// version lives and the two drift silently -- the manifest would keep announcing
/// an old wire version after a dependency bump, which is exactly the kind of
/// stale-but-plausible fact this repo has been bitten by. Cargo.lock is tracked,
/// so it is the resolved truth for the binary being built.
///
/// Returns None rather than guessing when the entry is absent: an unstated
/// version is an honest gap, a wrong one is a claim.
fn wire_crate_version(manifest_dir: &std::path::Path) -> Option<String> {
    let lock = manifest_dir.parent()?.parent()?.join("Cargo.lock");
    let text = std::fs::read_to_string(lock).ok()?;
    let mut in_pkg = false;
    for line in text.lines() {
        let line = line.trim();
        if line == "[[package]]" {
            in_pkg = false;
            continue;
        }
        if line == "name = \"cortexkit-provider-usage\"" {
            in_pkg = true;
            continue;
        }
        if in_pkg {
            if let Some(rest) = line.strip_prefix("version = \"") {
                return rest.strip_suffix('"').map(str::to_string);
            }
        }
    }
    None
}

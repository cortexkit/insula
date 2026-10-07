//! Every binary these tests spawn runs under a `ckdev-` name, never `ck-`.
//!
//! On this fleet a `ck-<name>` process means the production binary in
//! `~/.local/share/cortexkit/bin`. A test that spawns cargo's
//! `target/debug/ck-insula`, or the sibling daemon `ck-subc`, by its own path
//! would show in Activity Monitor as a second live module.
//! `common::ckdev_binary` re-exposes a binary as `ckdev-<name>`, and this scan
//! fails when a spawn path skips it.

use std::path::Path;

/// Expressions that name a `ck-*` binary the tests run.
const SPAWNED_BINARIES: [&str; 2] = ["CARGO_BIN_EXE_", "build_subc_daemon()"];

/// Every place in `source` that names a spawned binary without passing it
/// through `ckdev_binary`, as `line: text`.
fn unwrapped_spawns(source: &str) -> Vec<String> {
    let mut found = Vec::new();
    for needle in SPAWNED_BINARIES {
        for (at, _) in source.match_indices(needle) {
            let line_start = source[..at].rfind('\n').map_or(0, |i| i + 1);
            let line =
                &source[line_start..source[at..].find('\n').map_or(source.len(), |i| at + i)];
            // The helper's own definition names the build function, and doc or
            // line comments may mention either expression.
            if line.trim_start().starts_with("//") || line.contains("fn build_subc_daemon") {
                continue;
            }
            // The wrapping call must be in the same statement. A fixed look-back
            // window would let a neighbouring statement's `ckdev_binary(` excuse
            // a direct spawn on the next line.
            let statement_start = source[..at].rfind([';', '{', '}']).map_or(0, |i| i + 1);
            if !source[statement_start..at].contains("ckdev_binary(") {
                let number = source[..at].matches('\n').count() + 1;
                found.push(format!("{number}: {}", line.trim()));
            }
        }
    }
    found
}

#[test]
fn every_spawned_binary_is_exposed_under_a_ckdev_name() {
    let tests = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut scanned = 0;
    let mut offenders = Vec::new();
    for dir in [tests.join("it"), tests.join("common")] {
        for entry in std::fs::read_dir(&dir).expect("read the test sources") {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|ext| ext == "rs") && !path.ends_with("ckdev_names.rs")
            {
                let source = std::fs::read_to_string(&path).unwrap();
                scanned += 1;
                for hit in unwrapped_spawns(&source) {
                    offenders.push(format!("{}:{hit}", path.display()));
                }
            }
        }
    }
    // A scan that found no files would pass over everything.
    assert!(scanned >= 3, "scanned only {scanned} test source file(s)");
    assert!(
        offenders.is_empty(),
        "these spawn a ck-* binary under its own name; wrap it in common::ckdev_binary \
         so it runs as ckdev-<name>:\n{}",
        offenders.join("\n")
    );
}

/// The planted violation: the scan must catch a direct spawn of each binary,
/// and pass the wrapped forms the suites actually use.
#[test]
fn the_ckdev_scan_flags_a_planted_direct_spawn_and_passes_wrapped_ones() {
    // Each statement must be judged on its own. The last spawn here is direct,
    // but it follows a statement that does call `ckdev_binary(`. A scan that
    // searched a fixed number of characters back would find that earlier call
    // and wrongly let the direct spawn pass.
    let planted = r#"
        let a = Command::new(env!("CARGO_BIN_EXE_ck-insula"));
        let b = Command::new(&build_subc_daemon());
        let subc = ckdev_binary(&build_subc_daemon(), &rig, "subc");
        let c = PathBuf::from(env!("CARGO_BIN_EXE_ck-insula"));
    "#;
    assert_eq!(
        unwrapped_spawns(planted).len(),
        3,
        "{:?}",
        unwrapped_spawns(planted)
    );

    let wrapped = r#"
        // CARGO_BIN_EXE_ck-insula is mentioned in this comment.
        fn build_subc_daemon() -> PathBuf {
        let subc = ckdev_binary(&build_subc_daemon(), &rig, "subc");
        let module = ckdev_binary(
            Path::new(env!("CARGO_BIN_EXE_ck-insula")),
            &rig,
            "insula",
        );
    "#;
    assert!(
        unwrapped_spawns(wrapped).is_empty(),
        "{:?}",
        unwrapped_spawns(wrapped)
    );
}

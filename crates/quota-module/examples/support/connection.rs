//! Shared diagnostic connection discovery. `--subc <path>` bypasses environment
//! overrides and default candidates; remaining positional arguments retain each
//! example's provider-filter meaning.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

pub fn connection_and_args() -> (PathBuf, Vec<OsString>) {
    let result = parse_args(std::env::args_os().skip(1)).and_then(|(explicit, remaining)| {
        let candidates = cortexkit_paths::connection_file_candidates(
            explicit.as_deref(),
            &subc_transport::user_connection_token(),
        )
        .map_err(|error| error.to_string())?;
        select_existing(&candidates, Path::exists).map(|path| (path, remaining))
    });
    result.unwrap_or_else(|error| {
        eprintln!("{error}\nthe daemon must be running; nothing was checked");
        std::process::exit(2)
    })
}

fn parse_args(
    args: impl IntoIterator<Item = OsString>,
) -> Result<(Option<PathBuf>, Vec<OsString>), String> {
    let mut explicit = None;
    let mut remaining = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--subc" {
            let path = args
                .next()
                .filter(|path| !path.is_empty())
                .ok_or("--subc requires a connection-file path")?;
            if explicit.replace(PathBuf::from(path)).is_some() {
                return Err("--subc may be specified only once".to_string());
            }
        } else {
            remaining.push(arg);
        }
    }
    Ok((explicit, remaining))
}

fn select_existing(
    candidates: &[PathBuf],
    exists: impl Fn(&Path) -> bool,
) -> Result<PathBuf, String> {
    candidates
        .iter()
        .find(|path| exists(path))
        .cloned()
        .ok_or_else(|| {
            format!(
                "no daemon connection file found; tried:\n{}",
                candidates
                    .iter()
                    .map(|path| format!("  {}", path.display()))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortexkit_paths::EnvSource;

    struct Inputs {
        root: PathBuf,
    }
    impl EnvSource for Inputs {
        fn var_os(&self, key: &str) -> Option<OsString> {
            match key {
                "SUBC_CONNECTION_FILE" => Some(self.root.join("override.json").into_os_string()),
                _ => None,
            }
        }
        fn current_dir(&self) -> std::io::Result<PathBuf> {
            Ok(self.root.clone())
        }
    }

    #[test]
    fn explicit_connection_wins_without_consuming_the_provider_filter() {
        for args in [
            ["--subc", "chosen.json", "codex"],
            ["codex", "--subc", "chosen.json"],
        ] {
            let (explicit, remaining) = parse_args(args.into_iter().map(OsString::from)).unwrap();
            assert_eq!(remaining, vec![OsString::from("codex")]);
            let env = Inputs {
                root: std::env::temp_dir().join("diagnostic-connection"),
            };
            let paths = cortexkit_paths::with_env(&env)
                .connection_file_candidates(explicit.as_deref(), "fixture")
                .unwrap();
            assert_eq!(paths, vec![env.root.join("chosen.json")]);
        }
        assert!(parse_args([OsString::from("--subc")]).is_err());
    }

    #[test]
    fn discovery_selects_the_first_existing_candidate_or_lists_every_attempt() {
        let root = std::env::temp_dir().join("diagnostic-connection");
        let candidates = vec![
            root.join("runtime.json"),
            root.join("home.json"),
            root.join("temp.json"),
        ];
        assert_eq!(
            select_existing(&candidates, |path| path != candidates[0]).unwrap(),
            candidates[1]
        );
        let error = select_existing(&candidates, |_| false).unwrap_err();
        for path in &candidates {
            assert!(
                error.contains(&path.display().to_string()),
                "missing attempted candidate: {error}"
            );
        }
    }
}

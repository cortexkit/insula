//! Bounded reads for provider-owned local credential and state files.

use std::{
    fs::OpenOptions,
    io::{self, Read},
    path::Path,
};

// These sources are small JSON documents, but may include many accounts.
pub(crate) const MAX_BYTES: u64 = 4 * 1024 * 1024;

pub(crate) enum ReadError {
    Missing(io::Error),
    Io(io::Error),
    Refused(&'static str),
}

const NOT_REGULAR: ReadError = ReadError::Refused("not a regular file");
const TOO_LARGE: ReadError = ReadError::Refused("exceeds the 4 MiB local file limit");

/// A missing path is different from a path that we refused or could not read.
/// Links may point to regular files. Check the opened handle before reading,
/// because another process may replace the path or its target at any time.
pub(crate) fn read(path: &Path) -> Result<Vec<u8>, ReadError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // A FIFO, including one reached through a link, must not wait for a writer.
        options.custom_flags(libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::path::Prefix;
        if let Some(std::path::Component::Prefix(prefix)) = path.components().next() {
            match prefix.kind() {
                Prefix::DeviceNS(_) | Prefix::Verbatim(_) => return Err(NOT_REGULAR),
                Prefix::UNC(_, share) | Prefix::VerbatimUNC(_, share)
                    if share.eq_ignore_ascii_case("pipe")
                        || share.eq_ignore_ascii_case("mailslot") =>
                {
                    return Err(NOT_REGULAR);
                }
                _ => {}
            }
        }
        // Windows' ordinary open rejects directories before returning a handle.
        // Classify them as non-regular here, but still verify any opened handle.
        if std::fs::metadata(path).is_ok_and(|metadata| !metadata.is_file()) {
            return Err(NOT_REGULAR);
        }
    }

    let file = options.open(path).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => ReadError::Missing(error),
        _ => ReadError::Io(error),
    })?;
    let metadata = file.metadata().map_err(ReadError::Io)?;
    if !metadata.is_file() {
        return Err(NOT_REGULAR);
    }
    if metadata.len() > MAX_BYTES {
        return Err(TOO_LARGE);
    }

    // The opened file may still grow after the metadata check.
    read_bounded(file)
}

fn read_bounded(reader: impl Read) -> Result<Vec<u8>, ReadError> {
    let mut data = Vec::new();
    reader
        .take(MAX_BYTES + 1)
        .read_to_end(&mut data)
        .map_err(ReadError::Io)?;
    if data.len() as u64 > MAX_BYTES {
        return Err(TOO_LARGE);
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_stops_one_byte_past_the_limit() {
        let mut source = io::repeat(b'x').take(MAX_BYTES);
        assert_eq!(
            read_bounded(&mut source).ok().unwrap().len() as u64,
            MAX_BYTES
        );

        let mut source = io::repeat(b'x').take(MAX_BYTES + 2);
        assert!(matches!(
            read_bounded(&mut source),
            Err(ReadError::Refused(_))
        ));
        assert_eq!(source.limit(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_read_regular_targets_and_report_missing_targets() {
        use std::fs;

        let dir = crate::tests::ResetTempDir::with_prefix("insula-local-file", "symlink");
        let target = dir.path().join("target.json");
        let link = dir.path().join("link.json");
        fs::write(&target, b"{}").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert_eq!(read(&link).ok().unwrap(), b"{}");
        fs::remove_file(&target).unwrap();
        assert!(matches!(read(&link), Err(ReadError::Missing(_))));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_to_a_fifo_is_refused_without_waiting_for_a_writer() {
        use std::{ffi::CString, os::unix::ffi::OsStrExt, sync::mpsc, thread, time::Duration};

        let dir = crate::tests::ResetTempDir::with_prefix("insula-local-file", "fifo-link");
        let target = dir.path().join("fifo");
        let link = dir.path().join("link.json");
        let name = CString::new(target.as_os_str().as_bytes()).unwrap();
        // The pointer refers to a live, NUL-terminated fixture path.
        let status = unsafe { libc::mkfifo(name.as_ptr(), 0o600) };
        assert_eq!(status, 0, "creating a FIFO: {}", io::Error::last_os_error());
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let (send, receive) = mpsc::channel();
        let reader = thread::spawn(move || {
            let _ = send.send(read(&link));
        });
        let result = receive
            .recv_timeout(Duration::from_secs(1))
            .expect("reading the FIFO link must not wait for a writer");
        assert!(matches!(
            result,
            Err(ReadError::Refused("not a regular file"))
        ));
        reader.join().unwrap();
    }
}

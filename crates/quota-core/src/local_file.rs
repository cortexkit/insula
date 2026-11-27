//! Bounded reads for provider-owned local credential and state files.

use std::{
    fs::{self, Metadata, OpenOptions},
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
/// Inspect before opening to avoid special files, and inspect the opened
/// handle again because another process may have replaced the path meanwhile.
pub(crate) fn read(path: &Path) -> Result<Vec<u8>, ReadError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // In a rename race, don't follow a replacement symlink or wait for
        // the other end of a replacement FIFO.
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::{os::windows::fs::OpenOptionsExt, path::Prefix};
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
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }

    let metadata = fs::symlink_metadata(path).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => ReadError::Missing(error),
        _ => ReadError::Io(error),
    })?;
    require_regular(&metadata)?;

    let file = options.open(path).map_err(|error| {
        #[cfg(unix)]
        if error.raw_os_error() == Some(libc::ELOOP) {
            return NOT_REGULAR;
        }
        ReadError::Io(error)
    })?;
    let metadata = file.metadata().map_err(ReadError::Io)?;
    require_regular(&metadata)?;
    if metadata.len() > MAX_BYTES {
        return Err(TOO_LARGE);
    }

    // The opened file may still grow after the metadata check.
    read_bounded(file)
}

fn require_regular(metadata: &Metadata) -> Result<(), ReadError> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(NOT_REGULAR);
        }
    }
    if !metadata.is_file() {
        return Err(NOT_REGULAR);
    }
    Ok(())
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
    fn symlinks_are_refused_even_when_the_target_is_missing() {
        let dir = crate::tests::ResetTempDir::with_prefix("insula-local-file", "symlink");
        let target = dir.path().join("target.json");
        let link = dir.path().join("link.json");
        fs::write(&target, b"{}").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert_eq!(read(&target).ok().unwrap(), b"{}");
        assert!(matches!(read(&link), Err(ReadError::Refused(_))));
        fs::remove_file(&target).unwrap();
        assert!(matches!(read(&target), Err(ReadError::Missing(_))));
        assert!(matches!(read(&link), Err(ReadError::Refused(_))));
    }
}

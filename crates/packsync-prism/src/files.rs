//! Bounded, defensive file reads. Instance files come from disk *and* from restored snapshots, so
//! they are treated as untrusted: size caps, regular files only, no panics.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use packsync_core::error::{Error, Result};

/// Read a regular file of at most `max` bytes (following symlinks is allowed for reads: Prism
/// itself follows them).
pub fn read_bounded(path: &Path, max: usize) -> Result<Vec<u8>> {
    let md = std::fs::metadata(path).map_err(|e| Error::io(format!("reading {}", path.display()), e))?;
    if !md.is_file() {
        return Err(Error::invalid("instance file", format!("{} is not a regular file", path.display())));
    }
    if md.len() > max as u64 {
        return Err(Error::invalid("instance file", format!("{} is larger than {max} bytes", path.display())));
    }
    let f = File::open(path).map_err(|e| Error::io(format!("opening {}", path.display()), e))?;
    let mut buf = Vec::with_capacity(md.len() as usize);
    f.take(max as u64 + 1).read_to_end(&mut buf).map_err(|e| Error::io(format!("reading {}", path.display()), e))?;
    if buf.len() > max {
        return Err(Error::invalid("instance file", format!("{} is larger than {max} bytes", path.display())));
    }
    Ok(buf)
}

/// Like [`read_bounded`] but a missing file is `Ok(None)`.
pub fn read_bounded_opt(path: &Path, max: usize) -> Result<Option<Vec<u8>>> {
    match std::fs::metadata(path) {
        Ok(_) => read_bounded(path, max).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::io(format!("reading {}", path.display()), e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_small_files_and_rejects_big_or_irregular_ones() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("a.cfg");
        std::fs::write(&f, b"hello").unwrap();
        assert_eq!(read_bounded(&f, 10).unwrap(), b"hello");
        assert!(read_bounded(&f, 3).is_err());
        assert!(read_bounded(d.path(), 10).is_err(), "directories are not files");
        assert_eq!(read_bounded_opt(&d.path().join("missing"), 10).unwrap(), None);
        assert_eq!(read_bounded_opt(&f, 10).unwrap().unwrap(), b"hello");
    }
}

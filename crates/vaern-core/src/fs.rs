//! Native half of `vaern:fs`: whole-file reads, writes and appends.
//!
//! Each operation runs on Tokio's blocking pool and reports back through the
//! event loop channel, so a slow disk never stalls the JavaScript thread.
//! Every failure is a typed [`VaernError::Fs`] that names the stage that
//! failed, a POSIX-style code and the absolute path, because a script that
//! only sees "os error 2" cannot tell a missing file from a missing parent.
//!
//! No policy is applied here: a path is opened with the permissions of the
//! process. Policy hooks are reserved for the capability enforcer (P4,
//! v0.1.1), which will check the absolute path returned by [`resolve`] before
//! any open, so every operation already passes through that single point.

use crate::error::{Result, VaernError};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Largest file `readFile` accepts. It matches the JavaScript heap cap, so a
/// read that would exhaust the heap fails with `EFBIG` before it allocates.
pub const MAX_READ_BYTES: u64 = 256 * 1024 * 1024;

/// How `write` opens its target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteMode {
    /// Create the file or replace its contents (flag `"w"`).
    Truncate,
    /// Create the file, failing with `EEXIST` if it exists (flag `"wx"`).
    CreateNew,
    /// Create the file or add to its end (`appendFile`).
    Append,
}

/// One filesystem request from JavaScript.
#[derive(Debug)]
pub enum FsOp {
    Read {
        path: String,
        utf8: bool,
    },
    Write {
        path: String,
        data: Vec<u8>,
        mode: WriteMode,
    },
}

/// Successful result of an [`FsOp`].
#[derive(Debug, PartialEq, Eq)]
pub enum FsOutput {
    Bytes(Vec<u8>),
    Text(String),
    Written,
}

/// Runs `op` to completion on the calling thread.
pub fn run(op: FsOp) -> Result<FsOutput> {
    match op {
        FsOp::Read { path, utf8 } => {
            let path = resolve(&path)?;
            let bytes = read(&path)?;
            if utf8 {
                Ok(FsOutput::Text(String::from_utf8_lossy(&bytes).into_owned()))
            } else {
                Ok(FsOutput::Bytes(bytes))
            }
        }
        FsOp::Write { path, data, mode } => {
            let path = resolve(&path)?;
            write(&path, &data, mode)?;
            Ok(FsOutput::Written)
        }
    }
}

/// Turns a script-supplied path into the absolute path every later stage,
/// and every error, refers to. Relative paths resolve against the current
/// working directory.
pub fn resolve(path: &str) -> Result<PathBuf> {
    std::path::absolute(path).map_err(|err| fs_error("resolve", Path::new(path), &err))
}

fn read(path: &Path) -> Result<Vec<u8>> {
    // Windows refuses to open a directory with access denied; checking first
    // gives the same code on every platform.
    if path.is_dir() {
        return Err(is_a_directory(path));
    }
    let mut file = std::fs::File::open(path).map_err(|err| fs_error("open", path, &err))?;
    let meta = file
        .metadata()
        .map_err(|err| fs_error("open", path, &err))?;
    if meta.is_dir() {
        return Err(is_a_directory(path));
    }
    if meta.len() > MAX_READ_BYTES {
        return Err(too_large(path));
    }

    // The file can grow between the size check and the read, so the read
    // itself is bounded as well.
    let mut bytes = Vec::with_capacity(usize::try_from(meta.len()).unwrap_or(0));
    (&mut file)
        .take(MAX_READ_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| fs_error("read", path, &err))?;
    if bytes.len() as u64 > MAX_READ_BYTES {
        return Err(too_large(path));
    }
    Ok(bytes)
}

fn write(path: &Path, data: &[u8], mode: WriteMode) -> Result<()> {
    // Checked first for the same reason as in `read`.
    if path.is_dir() {
        return Err(is_a_directory(path));
    }
    let mut options = std::fs::OpenOptions::new();
    match mode {
        WriteMode::Truncate => options.write(true).create(true).truncate(true),
        WriteMode::CreateNew => options.write(true).create_new(true),
        WriteMode::Append => options.append(true).create(true),
    };
    let mut file = options
        .open(path)
        .map_err(|err| fs_error("open", path, &err))?;
    file.write_all(data)
        .and_then(|()| file.flush())
        .map_err(|err| fs_error("write", path, &err))
}

fn fs_error(stage: &str, path: &Path, err: &std::io::Error) -> VaernError {
    use std::io::ErrorKind;
    let (code, action) = match err.kind() {
        ErrorKind::NotFound => (
            "ENOENT",
            "Check that the file and every parent directory exist.",
        ),
        ErrorKind::PermissionDenied => (
            "EACCES",
            "Check the permissions of the file and its directory.",
        ),
        ErrorKind::AlreadyExists => (
            "EEXIST",
            "Remove the file first, or write with flag \"w\" to replace it.",
        ),
        ErrorKind::IsADirectory => ("EISDIR", "Pass the path of a file, not a directory."),
        ErrorKind::NotADirectory => (
            "ENOTDIR",
            "A parent in the path is a file; use a path through directories only.",
        ),
        ErrorKind::FileTooLarge => ("EFBIG", "Write less data at a time."),
        _ => ("EIO", "Retry, and check the disk and the path."),
    };
    VaernError::Fs {
        stage: stage.to_string(),
        code: code.to_string(),
        path: path.display().to_string(),
        message: err.to_string(),
        action: action.to_string(),
    }
}

fn is_a_directory(path: &Path) -> VaernError {
    VaernError::Fs {
        stage: "open".to_string(),
        code: "EISDIR".to_string(),
        path: path.display().to_string(),
        message: "the path is a directory".to_string(),
        action: "Pass the path of a file, not a directory.".to_string(),
    }
}

fn too_large(path: &Path) -> VaernError {
    VaernError::Fs {
        stage: "read".to_string(),
        code: "EFBIG".to_string(),
        path: path.display().to_string(),
        message: format!("the file is larger than the {MAX_READ_BYTES}-byte read limit"),
        action: "Read a smaller file; the limit matches the JavaScript heap cap.".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vaern-fs-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn code_of(err: VaernError) -> (String, String) {
        match err {
            VaernError::Fs { stage, code, .. } => (stage, code),
            other => panic!("expected an fs error, got {other}"),
        }
    }

    fn write_op(path: &Path, data: &[u8], mode: WriteMode) -> Result<FsOutput> {
        run(FsOp::Write {
            path: path.display().to_string(),
            data: data.to_vec(),
            mode,
        })
    }

    fn read_op(path: &Path, utf8: bool) -> Result<FsOutput> {
        run(FsOp::Read {
            path: path.display().to_string(),
            utf8,
        })
    }

    #[test]
    fn test_write_append_and_read_round_trip() {
        let dir = scratch("roundtrip");
        let file = dir.join("a.bin");
        write_op(&file, &[0, 255, 0xC3], WriteMode::Truncate).unwrap();
        write_op(&file, &[0xA9], WriteMode::Append).unwrap();
        assert_eq!(
            read_op(&file, false).unwrap(),
            FsOutput::Bytes(vec![0, 255, 0xC3, 0xA9])
        );
        write_op(&file, "é\u{1F600}".as_bytes(), WriteMode::Truncate).unwrap();
        assert_eq!(
            read_op(&file, true).unwrap(),
            FsOutput::Text("é\u{1F600}".to_string())
        );
    }

    #[test]
    fn test_invalid_utf8_reads_as_replacement_characters() {
        let dir = scratch("lossy");
        let file = dir.join("bad.txt");
        write_op(&file, &[b'a', 0xFF, b'b'], WriteMode::Truncate).unwrap();
        assert_eq!(
            read_op(&file, true).unwrap(),
            FsOutput::Text("a\u{FFFD}b".to_string())
        );
    }

    #[test]
    fn test_errors_name_stage_code_and_absolute_path() {
        let dir = scratch("errors");
        let missing = dir.join("missing.txt");
        let err = read_op(&missing, true).unwrap_err();
        let text = err.to_string();
        assert!(text.contains(&missing.display().to_string()), "{text}");
        assert_eq!(code_of(err), ("open".to_string(), "ENOENT".to_string()));

        let existing = dir.join("once.txt");
        write_op(&existing, b"1", WriteMode::CreateNew).unwrap();
        let err = write_op(&existing, b"2", WriteMode::CreateNew).unwrap_err();
        assert_eq!(code_of(err), ("open".to_string(), "EEXIST".to_string()));
        assert_eq!(
            read_op(&existing, true).unwrap(),
            FsOutput::Text("1".to_string())
        );

        let err = read_op(&dir, false).unwrap_err();
        assert_eq!(code_of(err), ("open".to_string(), "EISDIR".to_string()));
        let err = write_op(&dir, b"x", WriteMode::Truncate).unwrap_err();
        assert_eq!(code_of(err), ("open".to_string(), "EISDIR".to_string()));

        let err = write_op(&dir.join("no/such/dir.txt"), b"x", WriteMode::Truncate).unwrap_err();
        assert_eq!(code_of(err), ("open".to_string(), "ENOENT".to_string()));
    }

    #[test]
    fn test_read_refuses_files_above_the_heap_cap() {
        let dir = scratch("big");
        let file = dir.join("big.bin");
        // A sparse file: the size check runs before any byte is read.
        std::fs::File::create(&file)
            .unwrap()
            .set_len(MAX_READ_BYTES + 1)
            .unwrap();
        let err = read_op(&file, false).unwrap_err();
        assert_eq!(code_of(err), ("read".to_string(), "EFBIG".to_string()));
        std::fs::remove_file(&file).unwrap();
    }

    #[test]
    fn test_relative_paths_resolve_against_the_working_directory() {
        let resolved = resolve("some/file.txt").unwrap();
        assert!(resolved.is_absolute());
        assert!(resolved.starts_with(std::env::current_dir().unwrap()));
    }
}

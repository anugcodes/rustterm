//! Durable writes for the small config files.
//!
//! Both settings and saved sessions use the same pattern: write a sibling temp
//! file, flush it to disk, then rename it over the target. The temp name
//! includes the pid and a counter so two running instances cannot collide.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

/// Atomically replace `path` with `bytes`.
///
/// The parent directory is created if needed. A failed write removes the temp
/// file and leaves the previous contents of `path` untouched.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let Some(parent) = path.parent() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path has no parent directory",
        ));
    };
    fs::create_dir_all(parent)?;

    let tmp = temp_path(path)?;
    let result = (|| {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        // Flush the bytes before the rename makes the new file visible.
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn temp_path(path: &Path) -> io::Result<PathBuf> {
    let Some(name) = path.file_name() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path has no file name",
        ));
    };
    let mut name = name.to_os_string();
    name.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    Ok(path.with_file_name(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("rustterm-persist-test-{}-{id}", std::process::id()));
            fs::create_dir_all(&path).expect("create temp dir");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn replaces_the_file_and_leaves_no_temp_behind() {
        let dir = TempDir::new();
        let path = dir.0.join("settings.toml");
        fs::write(&path, "old").unwrap();

        atomic_write(&path, b"new").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");

        let leftovers: Vec<_> = fs::read_dir(&dir.0)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name() != "settings.toml")
            .collect();
        assert!(leftovers.is_empty(), "temp file was not cleaned up");
    }

    #[test]
    fn temp_names_are_unique() {
        let dir = TempDir::new();
        let path = dir.0.join("sessions.toml");
        let first = temp_path(&path).unwrap();
        let second = temp_path(&path).unwrap();
        assert_ne!(first, second);
        assert_eq!(first.parent(), Some(dir.0.as_path()));
    }
}

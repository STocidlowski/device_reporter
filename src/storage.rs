//! Private, crash-resistant file replacement shared by settings and the outbox.
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

pub fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = private_parent(path)?;
    let tmp = parent.join(format!(".dr-{}.tmp", uuid::Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)?;
        // Linux/Pi: persist the directory entry as well as the file contents.
        #[cfg(unix)]
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn private_parent(path: &Path) -> io::Result<&Path> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(parent)?;
    Ok(parent)
}

pub fn lock_exclusive(path: &Path) -> io::Result<fs::File> {
    private_parent(path)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    file.try_lock().map_err(|_| {
        io::Error::other("another process owns this state file; stop the service first")
    })?;
    Ok(file)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn replacement_preserves_old_file_when_parent_is_invalid() {
        let dir = std::env::temp_dir().join(format!("dr-storage-{}", uuid::Uuid::new_v4()));
        let path = dir.join("private.json");
        write_private(&path, b"first").unwrap();
        assert!(write_private(&path.join("bad"), b"second").is_err());
        assert_eq!(fs::read(&path).unwrap(), b"first");
        write_private(&path, b"second").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"second");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        fs::remove_dir_all(dir).unwrap();
    }
}

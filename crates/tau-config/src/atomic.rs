//! Atomic file write helper that follows symlinks at the destination.

use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Atomically write `contents` to `path` by creating a randomized sibling
/// temporary file and renaming it over the destination.
///
/// If `path` is a symlink, the symlink's target is replaced rather than the
/// symlink itself — preserving user-managed indirection (e.g. dotfile
/// managers).
///
/// Existing-file permissions are preserved across the replace. If the
/// destination does not exist, `default_permissions` is applied to the new
/// file (use this to enforce e.g. `0o600` for credential files); pass `None`
/// to let the umask decide.
pub fn atomic_write_following_symlink(
    path: &Path,
    contents: &[u8],
    default_permissions: Option<Permissions>,
) -> io::Result<()> {
    let destination = symlink_target_or_path(path)?;

    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }

    let permissions = fs::metadata(&destination)
        .ok()
        .map(|metadata| metadata.permissions())
        .or(default_permissions);

    let mut temp_path = destination.clone();
    loop {
        let suffix: u64 = rand::random();
        let file_name = destination
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file");
        temp_path.set_file_name(format!(".{file_name}.{suffix:016x}.tmp"));

        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
            Ok(mut file) => {
                if let Some(permissions) = permissions.clone() {
                    file.set_permissions(permissions)?;
                }
                file.write_all(contents)?;
                file.sync_all()?;
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }

    if let Err(error) = fs::rename(&temp_path, &destination) {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }

    sync_parent_dir(&destination)?;

    Ok(())
}

fn sync_parent_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

fn symlink_target_or_path(path: &Path) -> io::Result<PathBuf> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(path.to_path_buf()),
        Err(error) => return Err(error),
    };

    if !metadata.file_type().is_symlink() {
        return Ok(path.to_path_buf());
    }

    let target = fs::read_link(path)?;
    if target.is_absolute() {
        Ok(target)
    } else if let Some(parent) = path.parent() {
        Ok(parent.join(target))
    } else {
        Ok(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(unix)]
    fn replaces_symlink_target_and_preserves_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let temp_dir = tempfile::tempdir().expect("tempdir");
        let target = temp_dir.path().join("target.json5");
        let link = temp_dir.path().join("models.json5");
        std::fs::write(&target, b"{}").expect("write target");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        std::fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).expect("set perms");

        atomic_write_following_symlink(&link, b"{\"providers\":{}}", None).expect("atomic write");

        assert!(
            fs::symlink_metadata(&link)
                .expect("symlink metadata")
                .file_type()
                .is_symlink(),
            "the symlink itself must not be replaced"
        );
        let body = std::fs::read_to_string(&target).expect("read");
        assert!(body.contains("providers"));
        assert_eq!(
            fs::metadata(&target)
                .expect("target metadata")
                .permissions()
                .mode()
                & 0o777,
            0o640,
            "existing permissions on the target file are preserved"
        );
    }

    #[test]
    #[cfg(unix)]
    fn applies_default_permissions_when_file_is_new() {
        use std::os::unix::fs::PermissionsExt;

        let temp_dir = tempfile::tempdir().expect("tempdir");
        let path = temp_dir.path().join("auth.json");

        atomic_write_following_symlink(&path, b"{}", Some(fs::Permissions::from_mode(0o600)))
            .expect("atomic write");

        assert_eq!(
            fs::metadata(&path).expect("metadata").permissions().mode() & 0o777,
            0o600,
        );
    }
}

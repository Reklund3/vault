//! Best-effort permission hardening for everything vault writes under
//! `~/.vault/`. Unix-only chmod; Windows inherits the user-profile ACL.
//! Shared by the TEI launcher (tei.pid / tei.log) and the hook logger
//! (hook.log) so the 0700/0600 posture stays in one place.

use std::path::Path;

/// Best-effort `0700` on a directory vault owns.
#[cfg(unix)]
pub(crate) fn harden_dir(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
pub(crate) fn harden_dir(_path: &Path) {}

/// Open a file vault owns for appending, creating it `0600` **at creation**.
///
/// [`harden_file`] can only chmod a file that already exists, so a create-then-
/// chmod sequence leaves a window in which the file carries whatever the
/// process umask allowed — commonly `0644`, i.e. world-readable. `hook.log`
/// records prompt metadata and `tei.log` captures server output, so on a shared
/// host that window is a real disclosure. Passing the mode to `open` closes it.
///
/// The mode applies **only when the file is created**; an existing file keeps
/// its bits. Callers therefore still call [`harden_file`] afterwards, which is
/// what repairs a file an older vault left behind at `0644`.
#[cfg(all(unix, feature = "cli"))]
pub(crate) fn open_private_append(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
}

#[cfg(all(not(unix), feature = "cli"))]
pub(crate) fn open_private_append(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
}

/// Write `contents` to a file vault owns, creating it `0600` at creation.
/// The truncating counterpart to [`open_private_append`]; same rationale, same
/// caveat about pre-existing files.
#[cfg(feature = "cli")]
pub(crate) fn write_private(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    let mut f = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(path)?
    };
    #[cfg(not(unix))]
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)?;
    f.write_all(contents)
}

/// Best-effort `0600` on a file vault writes into `~/.vault/`.
#[cfg(unix)]
pub(crate) fn harden_file(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
pub(crate) fn harden_file(_path: &Path) {}

#[cfg(all(test, unix, feature = "cli"))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("vault-fs-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The file must be `0600` the instant it exists, with no `harden_file`
    /// call in between. Creating it under the ambient umask and chmod-ing after
    /// leaves it world-readable for that window, and `hook.log` carries prompt
    /// metadata.
    #[test]
    fn an_appended_file_is_private_from_creation() {
        let dir = scratch("append");
        let path = dir.join("hook.log");

        let mut f = open_private_append(&path).unwrap();
        {
            use std::io::Write;
            f.write_all(b"line\n").unwrap();
        }

        assert_eq!(
            mode_of(&path),
            0o600,
            "created with the umask applied, not 0600"
        );
    }

    #[test]
    fn a_written_file_is_private_from_creation() {
        let dir = scratch("write");
        let path = dir.join("tei.pid");

        write_private(&path, b"12345").unwrap();

        assert_eq!(mode_of(&path), 0o600);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "12345");
    }

    /// The mode is a *creation* mode, so it cannot repair a file that already
    /// exists — which is exactly why callers still run `harden_file`. Pinned so
    /// nobody drops those calls believing the helper covers it.
    #[test]
    fn an_existing_loose_file_keeps_its_mode_until_hardened() {
        let dir = scratch("existing");
        let path = dir.join("hook.log");
        std::fs::write(&path, b"old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        drop(open_private_append(&path).unwrap());
        assert_eq!(mode_of(&path), 0o644, "creation mode cannot chmod");

        harden_file(&path);
        assert_eq!(mode_of(&path), 0o600, "harden_file is what repairs it");
    }

    #[test]
    fn write_private_truncates_rather_than_appending() {
        let dir = scratch("truncate");
        let path = dir.join("tei.pid");
        write_private(&path, b"999999").unwrap();
        write_private(&path, b"123").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "123");
    }
}

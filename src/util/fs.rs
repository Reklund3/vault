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

/// Best-effort `0600` on a file vault writes into `~/.vault/`.
#[cfg(unix)]
pub(crate) fn harden_file(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
pub(crate) fn harden_file(_path: &Path) {}

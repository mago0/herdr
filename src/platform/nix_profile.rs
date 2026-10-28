//! The profile link a Nix-installed herdr was started through.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

const NIX_STORE: &str = "/nix/store";

/// The link outside the Nix store that `invoked` (argv[0]) names, when `executable` is a store
/// path and the link resolves to a herdr in the store. A profile upgrade moves the link, not
/// the store path, so the link starts the version the profile holds at that moment.
pub(super) fn launch_link(
    executable: &Path,
    invoked: Option<&Path>,
    search_path: Option<&OsStr>,
) -> Option<PathBuf> {
    if !executable.starts_with(NIX_STORE) {
        return None;
    }
    let invoked = invoked?;
    let link = if invoked.is_absolute() {
        invoked.to_path_buf()
    } else if invoked.components().count() == 1 {
        std::env::split_paths(search_path?)
            .map(|dir| dir.join(invoked))
            .find(|candidate| candidate.is_file())?
    } else {
        // A relative path with a directory depends on a cwd that may have changed since.
        return None;
    };
    if link.starts_with(NIX_STORE) {
        return None;
    }
    let target = link.canonicalize().ok()?;
    (target.starts_with(NIX_STORE) && target.file_name() == executable.file_name()).then_some(link)
}

#[cfg(all(test, unix))]
mod tests {
    use super::launch_link;
    use std::path::{Path, PathBuf};

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("herdr-nix-profile-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A fake store under a temp dir cannot pass the `/nix/store` check, so these tests use
    /// the parts that do not need one, plus a real store path when the machine has one.
    #[test]
    fn a_binary_outside_the_store_keeps_its_own_path() {
        let exe = Path::new("/home/user/.local/lib/herdr-fork/herdr");
        assert_eq!(
            launch_link(exe, Some(Path::new("/usr/bin/herdr")), None),
            None
        );
    }

    #[test]
    fn a_store_binary_started_by_its_store_path_has_no_link() {
        let exe = Path::new("/nix/store/abc-herdr-0.9.5/bin/herdr");
        assert_eq!(launch_link(exe, Some(exe), None), None);
    }

    #[test]
    fn a_relative_argv0_with_a_directory_is_not_trusted() {
        let exe = Path::new("/nix/store/abc-herdr-0.9.5/bin/herdr");
        assert_eq!(launch_link(exe, Some(Path::new("bin/herdr")), None), None);
    }

    #[test]
    fn a_link_to_a_file_outside_the_store_is_not_a_profile_link() {
        let dir = temp_dir("outside");
        let real = dir.join("herdr-real");
        std::fs::write(&real, b"").unwrap();
        let link = dir.join("herdr");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let exe = Path::new("/nix/store/abc-herdr-0.9.5/bin/herdr");
        assert_eq!(launch_link(exe, Some(&link), None), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Runs where a Nix store holds some executable: a link to it, found by name on PATH or
    /// by its absolute path, is the launch link.
    #[test]
    fn a_profile_link_into_the_store_is_the_launch_link() {
        let Some(store_bin) = std::fs::read_dir("/nix/store").ok().and_then(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path().join("bin/bash"))
                .find(|path| path.is_file())
        }) else {
            return;
        };
        let dir = temp_dir("profile");
        let link = dir.join("bash");
        std::os::unix::fs::symlink(&store_bin, &link).unwrap();
        assert_eq!(
            launch_link(&store_bin, Some(&link), None),
            Some(link.clone())
        );
        assert_eq!(
            launch_link(&store_bin, Some(Path::new("bash")), Some(dir.as_os_str())),
            Some(link)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

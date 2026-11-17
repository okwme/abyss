//! Cache location and directory preparation (tasks 9b, 9c).

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use super::*;

/// A temporary home with `Library/Application Support`, as on macOS.
fn home() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("Library/Application Support")).unwrap();
    home
}

fn real(home: &Path) -> CacheLocation {
    CacheLocation::real(Some(home.to_path_buf())).unwrap()
}

fn mode(path: &Path) -> u32 {
    std::fs::symlink_metadata(path)
        .unwrap()
        .permissions()
        .mode()
        & 0o777
}

#[test]
fn the_real_cache_is_in_application_support() {
    let location = CacheLocation::real(Some("/Users/someone".into())).unwrap();
    assert_eq!(
        location.dir(),
        Path::new("/Users/someone/Library/Application Support/dev.abyss/cache")
    );
}

#[test]
fn an_unknown_or_relative_home_has_no_cache() {
    assert!(matches!(
        CacheLocation::real(None),
        Err(StorageError::NoHome)
    ));
    for bad in ["", "relative/home", "~"] {
        assert!(matches!(
            CacheLocation::real(Some(bad.into())),
            Err(StorageError::NoHome)
        ));
    }
    assert!(config(CacheLocation::real(None)).is_none());
}

#[test]
fn prepare_creates_private_directories() {
    let home = home();
    let location = real(home.path());
    let dir = prepare(&location).unwrap();
    assert_eq!(dir, location.dir());
    let app = home.path().join("Library/Application Support/dev.abyss");
    assert_eq!(mode(&app), 0o700);
    assert_eq!(mode(&dir), 0o700);
    // Idempotent: a second launch finds them in place.
    assert_eq!(prepare(&location).unwrap(), dir);
    let config = config(Ok(location)).unwrap();
    assert_eq!(config.dir, dir);
}

#[test]
fn prepare_narrows_a_wider_mode() {
    let home = home();
    let app = home.path().join("Library/Application Support/dev.abyss");
    std::fs::create_dir(&app).unwrap();
    std::fs::set_permissions(&app, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::create_dir(app.join("cache")).unwrap();
    std::fs::set_permissions(app.join("cache"), std::fs::Permissions::from_mode(0o777)).unwrap();
    let dir = prepare(&real(home.path())).unwrap();
    assert_eq!(mode(&app), 0o700);
    assert_eq!(mode(&dir), 0o700);
}

/// Adds an ACL entry with `chmod +a`.
#[cfg(target_os = "macos")]
fn add_acl(path: &Path, entry: &str) {
    let status = std::process::Command::new("/bin/chmod")
        .arg("+a")
        .arg(entry)
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success());
    assert!(acl::has_entries(path).unwrap());
}

/// `ls -led` lists ACL entries as indented `N: ...` lines.
#[cfg(target_os = "macos")]
fn listed_entries(path: &Path) -> usize {
    let out = std::process::Command::new("/bin/ls")
        .arg("-led")
        .arg(path)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .skip(1)
        .filter(|l| l.trim_start().starts_with(char::is_numeric))
        .count()
}

/// Round 26, finding 3: an `everyone` allow entry, on either app-owned
/// directory or inherited from Application Support, does not survive; the
/// ACL of Application Support itself is left alone.
#[cfg(target_os = "macos")]
#[test]
fn acl_entries_on_app_directories_are_removed() {
    let home = home();
    let support = home.path().join("Library/Application Support");
    let app = support.join("dev.abyss");
    std::fs::create_dir(&app).unwrap();
    std::fs::create_dir(app.join("cache")).unwrap();
    add_acl(&app, "everyone allow list,search,read");
    add_acl(&app.join("cache"), "everyone allow list,search,read,write");
    let inherit = "everyone allow list,search,read,file_inherit,directory_inherit";
    add_acl(&support, inherit);
    let dir = prepare(&real(home.path())).unwrap();
    for path in [&app, &dir] {
        assert!(!acl::has_entries(path).unwrap(), "{path:?}");
        assert_eq!(listed_entries(path), 0, "{path:?}");
    }
    assert_eq!(listed_entries(&support), 1, "Application Support untouched");

    // Created fresh under the inheriting parent: the inherited entry goes too.
    let home = self::home();
    let support = home.path().join("Library/Application Support");
    add_acl(&support, inherit);
    let dir = prepare(&real(home.path())).unwrap();
    assert!(!acl::has_entries(&support.join("dev.abyss")).unwrap());
    assert!(!acl::has_entries(&dir).unwrap());
}

#[test]
fn a_symlinked_component_is_refused() {
    let home = home();
    let elsewhere = tempfile::tempdir().unwrap();
    std::fs::set_permissions(elsewhere.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let app = home.path().join("Library/Application Support/dev.abyss");
    // The cache directory is a symlink to a real directory.
    std::fs::create_dir(&app).unwrap();
    std::os::unix::fs::symlink(elsewhere.path(), app.join("cache")).unwrap();
    assert!(matches!(
        prepare(&real(home.path())),
        Err(StorageError::Symlink("cache"))
    ));
    // The target's mode was not touched through the link.
    assert_eq!(mode(elsewhere.path()), 0o755);
    assert!(config(Ok(real(home.path()))).is_none());

    // The app directory itself is a symlink.
    let home = self::home();
    let app = home.path().join("Library/Application Support/dev.abyss");
    std::os::unix::fs::symlink(elsewhere.path(), &app).unwrap();
    assert!(matches!(
        prepare(&real(home.path())),
        Err(StorageError::Symlink("dev.abyss"))
    ));
    assert!(!elsewhere.path().join("cache").exists());
}

#[test]
fn a_file_in_place_or_a_missing_root_has_no_cache() {
    let home = home();
    let app = home.path().join("Library/Application Support/dev.abyss");
    std::fs::write(&app, b"").unwrap();
    assert!(matches!(
        prepare(&real(home.path())),
        Err(StorageError::NotADirectory("dev.abyss"))
    ));
    // No `Library/Application Support`: nothing is created above the root.
    let bare = tempfile::tempdir().unwrap();
    assert!(matches!(
        prepare(&real(bare.path())),
        Err(StorageError::Root(_))
    ));
    assert!(!bare.path().join("Library").exists());
}

#[test]
fn errors_never_name_the_path() {
    let error = StorageError::Io("cache", std::io::Error::other("boom"));
    assert_eq!(error.to_string(), "\"cache\": boom");
}

#[cfg(feature = "fake")]
mod fake {
    use std::ffi::OsString;

    use super::*;

    fn fake(tmpdir: &Path, home: &Path) -> CacheLocation {
        let tmp = Some(tmpdir.as_os_str().to_owned());
        CacheLocation::fake(Some("1".as_ref()), tmp, Some(home.to_path_buf())).unwrap()
    }

    #[test]
    fn the_fake_has_no_cache_unless_asked() {
        let tmp = Some(OsString::from("/tmp/x"));
        assert_eq!(CacheLocation::fake(None, tmp.clone(), None), None);
        for value in ["", "0", "true", "yes", " 1"] {
            assert_eq!(
                CacheLocation::fake(Some(value.as_ref()), tmp.clone(), None),
                None,
                "{value:?}"
            );
        }
        // Asked, but without an absolute TMPDIR.
        assert_eq!(CacheLocation::fake(Some("1".as_ref()), None, None), None);
        let relative = Some("rel".into());
        assert_eq!(
            CacheLocation::fake(Some("1".as_ref()), relative, None),
            None
        );
    }

    #[test]
    fn the_fake_cache_is_never_the_real_one() {
        let home = home();
        let support = home.path().join("Library/Application Support");
        // Even with TMPDIR pointed at Application Support.
        let fake = fake(&support, home.path());
        let real = real(home.path());
        assert_ne!(fake.dir(), real.dir());
        assert!(!fake.dir().starts_with(real.dir()));
        let dir = prepare(&fake).unwrap();
        assert!(dir.starts_with(support.join("dev.abyss.fake")));
        assert_eq!(mode(&dir), 0o700);
        assert!(!real.dir().exists(), "the fake touched the real cache");
    }

    /// Round 26, finding 2: a `TMPDIR` inside the real app directory, by
    /// path or through a symlink, is refused before anything is created.
    #[test]
    fn a_tmpdir_inside_the_real_tree_is_refused() {
        let home = home();
        let real_dir = prepare(&real(home.path())).unwrap();
        let before = std::fs::read_dir(&real_dir).unwrap().count();
        assert!(matches!(
            prepare(&fake(&real_dir, home.path())),
            Err(StorageError::Overlap)
        ));
        let link = home.path().join("tmp-link");
        std::os::unix::fs::symlink(&real_dir, &link).unwrap();
        assert!(matches!(
            prepare(&fake(&link, home.path())),
            Err(StorageError::Overlap)
        ));
        assert_eq!(std::fs::read_dir(&real_dir).unwrap().count(), before);
    }

    /// Round 29, finding 1: without a known home the real tree can't be
    /// located, so isolation can't be checked: no fake cache, even with a
    /// `TMPDIR` inside an existing real cache.
    #[test]
    fn an_unknown_home_disables_the_fake_cache() {
        let home = home();
        let real_dir = prepare(&real(home.path())).unwrap();
        let tmp = Some(real_dir.clone().into_os_string());
        assert_eq!(CacheLocation::fake(Some("1".as_ref()), tmp, None), None);
        assert_eq!(std::fs::read_dir(&real_dir).unwrap().count(), 0);
    }

    /// Round 29, finding 1: only a real directory that does not exist is
    /// known not to contain the fake root. One that can't be resolved for
    /// another reason (here, an unsearchable Application Support) refuses.
    #[test]
    fn an_unresolvable_real_tree_refuses_the_fake_cache() {
        let home = home();
        let support = home.path().join("Library/Application Support");
        prepare(&real(home.path())).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let location = fake(tmp.path(), home.path());
        std::fs::set_permissions(&support, std::fs::Permissions::from_mode(0o000)).unwrap();
        let prepared = prepare(&location);
        std::fs::set_permissions(&support, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            matches!(prepared, Err(StorageError::Overlap)),
            "{prepared:?}"
        );
        // A real tree that does not exist yet cannot contain it.
        let fresh = self::home();
        assert!(prepare(&fake(tmp.path(), fresh.path())).is_ok());
    }

    /// Round 29, finding 2: the run directory is removed at quit once the
    /// cache is closed, and kept while a worker still has it open.
    #[test]
    fn the_fake_run_directory_is_removed_once_closed() {
        let home = home();
        let tmp = tempfile::tempdir().unwrap();
        let location = fake(tmp.path(), home.path());
        let dir = prepare(&location).unwrap();
        let run = location.parent();
        let key = abyss_core::cache::CacheKey::generate().unwrap();
        let cache = abyss_core::cache::Cache::open(&dir, &key).unwrap();
        let none = std::time::Duration::ZERO;
        assert!(!remove_run(&run, &dir, none), "removed while open");
        assert!(dir.join("cache.db").exists());
        drop(cache);
        assert!(remove_run(&run, &dir, none));
        assert!(!run.exists());
        assert!(
            tmp.path().join("dev.abyss.fake").exists(),
            "only the run goes"
        );
    }

    /// Round 26, finding 2: each fake run gets a fresh directory, so a
    /// planted `cache.db` (here a symlink to the real database) is never
    /// adopted; an existing run directory is refused, not reused.
    #[test]
    fn each_fake_run_gets_a_fresh_directory() {
        let home = home();
        let tmp = tempfile::tempdir().unwrap();
        let first = fake(tmp.path(), home.path());
        let dir = prepare(&first).unwrap();
        let real_db = real(home.path()).dir().join("cache.db");
        std::os::unix::fs::symlink(&real_db, dir.join("cache.db")).unwrap();
        let second = fake(tmp.path(), home.path());
        assert_ne!(second.dir(), first.dir());
        let fresh = prepare(&second).unwrap();
        assert_eq!(std::fs::read_dir(&fresh).unwrap().count(), 0);
        // The same run again: refused rather than reused.
        assert!(matches!(prepare(&first), Err(StorageError::Exists(_))));
    }
}

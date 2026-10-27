use std::env;
use std::path::PathBuf;

/// Process-level configuration from environment variables. Runtime-editable
/// options live in the `settings` table instead (see `db::settings`).
#[derive(Debug, Clone)]
pub struct Config {
    /// `DATA_DIR` (default `./data`): database, session key.
    pub data_dir: PathBuf,
    /// `0.0.0.0:$PORT` (default port 3001).
    pub bind_addr: String,
}

impl Config {
    pub fn from_env() -> Self {
        let data_dir: PathBuf = env::var("DATA_DIR")
            .unwrap_or_else(|_| "./data".into())
            .into();
        let port = env::var("PORT").unwrap_or_else(|_| "3001".into());
        Self {
            data_dir,
            bind_addr: format!("0.0.0.0:{port}"),
        }
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("vigilant-vole.db")
    }
}

impl Config {
    /// Keeps other local users out of what the data directory holds: the
    /// database (password hashes, sealed secrets), the backups of it and the
    /// scratch space for downloads and uploads. Unix only; the session key
    /// is already created that way. A path that can't be changed (a mount
    /// owned by someone else) is logged and left as it is.
    pub fn restrict_permissions(&self) {
        let db = self.db_path();
        let mut files = vec![
            db.with_extension("db-wal"),
            db.with_extension("db-shm"),
            db,
            self.data_dir.join(crate::config_io::RESTORE_PENDING_FILE),
        ];
        for dir in ["backups", "tmp"] {
            let dir = self.data_dir.join(dir);
            if let Ok(entries) = std::fs::read_dir(&dir) {
                files.extend(entries.flatten().map(|e| e.path()));
            }
            files.push(dir);
        }
        for path in files.iter().filter(|p| p.exists()) {
            if let Err(err) = owner_only(path) {
                tracing::warn!(path = %path.display(), %err, "couldn't restrict the permissions");
            }
        }
    }
}

/// Lets only the owner use `path`: mode 0700 for a directory, 0600 for a
/// file. Does nothing off unix.
pub fn owner_only(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = if path.is_dir() { 0o700 } else { 0o600 };
        if std::fs::metadata(path)?.permissions().mode() & 0o777 != mode {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Creates `dir` (and its parents) for the owner only.
pub fn create_private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    owner_only(dir)
}

/// Creates the new, empty file `path` readable by its owner only, so that
/// what is written into it never exists with wider permissions.
pub fn create_private_file(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path)
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    fn mode(path: &std::path::Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("vv-perms-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn private_files_and_directories_are_for_the_owner() {
        let dir = scratch("create");
        let file = dir.join("snapshot.db");
        drop(create_private_file(&file).unwrap());
        assert_eq!(mode(&file), 0o600);
        assert!(create_private_file(&file).is_err(), "never reuses a file");
        let sub = dir.join("tmp");
        create_private_dir(&sub).unwrap();
        assert_eq!(mode(&sub), 0o700);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_database_and_its_backups_are_restricted_at_startup() {
        let dir = scratch("startup");
        let config = Config {
            data_dir: dir.clone(),
            bind_addr: String::new(),
        };
        std::fs::create_dir_all(dir.join("backups")).unwrap();
        std::fs::create_dir_all(dir.join("tmp")).unwrap();
        let files = [
            config.db_path(),
            dir.join("vigilant-vole.db-wal"),
            dir.join("backups").join("old.db"),
            dir.join("tmp").join("snapshot-x.db"),
        ];
        for f in &files {
            std::fs::write(f, b"x").unwrap();
            std::fs::set_permissions(f, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        std::fs::set_permissions(dir.join("tmp"), std::fs::Permissions::from_mode(0o755)).unwrap();
        config.restrict_permissions();
        for f in &files {
            assert_eq!(mode(f), 0o600, "{}", f.display());
        }
        assert_eq!(mode(&dir.join("tmp")), 0o700);
        assert_eq!(mode(&dir.join("backups")), 0o700);
        // Nothing to restrict is fine.
        Config {
            data_dir: dir.join("missing"),
            bind_addr: String::new(),
        }
        .restrict_permissions();
        let _ = std::fs::remove_dir_all(&dir);
    }
}

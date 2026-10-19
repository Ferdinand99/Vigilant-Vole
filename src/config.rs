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
        let data_dir: PathBuf = env::var("DATA_DIR").unwrap_or_else(|_| "./data".into()).into();
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

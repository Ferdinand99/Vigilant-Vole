use deadpool_sqlite::{Config as PoolConfig, Pool, Runtime};
use rust_embed::Embed;
use std::path::Path;

pub mod models;
pub mod monitors;
pub mod notifications;
pub mod users;

#[derive(Embed)]
#[folder = "migrations/"]
struct Migrations;

pub fn build_pool(db_path: &Path) -> Pool {
    PoolConfig::new(db_path)
        .create_pool(Runtime::Tokio1)
        .expect("failed to create sqlite pool")
}

pub async fn run_migrations(pool: &Pool) {
    let conn = pool.get().await.expect("failed to get db connection");
    let mut names: Vec<_> = Migrations::iter().collect();
    names.sort();
    for name in names {
        let file = Migrations::get(&name).expect("embedded migration missing");
        let sql = std::str::from_utf8(&file.data)
            .expect("migration is not valid utf8")
            .to_string();
        conn.interact(move |conn| conn.execute_batch(&sql))
            .await
            .unwrap_or_else(|e| panic!("migration task {name} panicked: {e}"))
            .unwrap_or_else(|e| panic!("migration {name} failed: {e}"));
    }
}

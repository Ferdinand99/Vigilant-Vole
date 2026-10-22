//! The database schema after migrations, on a fresh database and on every
//! release's database, plus the migration list and the pre-migration backup
//! file name.

use insta::assert_snapshot;
use rusqlite::Connection;
use vigilant_vole::db;

use crate::common::{DataDir, applied_migrations, fixtures, open_ro};
use crate::norm;

/// `sqlite_master` (tables, indexes, triggers, views; SQL with LF line
/// endings), then every table's columns and indexes, all sorted.
fn dump(conn: &Connection) -> String {
    let mut out = String::new();
    let mut stmt = conn
        .prepare(
            "SELECT type, name, tbl_name, sql FROM sqlite_master
             WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name",
        )
        .unwrap();
    let objects: Vec<(String, String, String, Option<String>)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    for (kind, name, table, sql) in &objects {
        out.push_str(&format!(
            "-- {kind} {name} (on {table})\n{};\n\n",
            sql.as_deref().unwrap_or("<auto>").replace("\r\n", "\n")
        ));
    }
    out.push_str("-- columns: cid name type notnull default pk\n");
    for (kind, name, _, _) in &objects {
        if kind != "table" {
            continue;
        }
        let mut stmt = conn
            .prepare(&format!(
                "SELECT cid, name, type, \"notnull\", dflt_value, pk FROM pragma_table_info('{name}') ORDER BY cid"
            ))
            .unwrap();
        let cols: Vec<String> = stmt
            .query_map([], |r| {
                Ok(format!(
                    "  {} {} {} {} {} {}",
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, Option<String>>(4)?.unwrap_or_else(|| "-".into()),
                    r.get::<_, i64>(5)?
                ))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        out.push_str(&format!("{name}\n{}\n", cols.join("\n")));
        let mut stmt = conn
            .prepare(&format!(
                "SELECT name, \"unique\", origin, partial FROM pragma_index_list('{name}') ORDER BY name"
            ))
            .unwrap();
        let idx: Vec<String> = stmt
            .query_map([], |r| {
                Ok(format!(
                    "  index {} unique={} origin={} partial={}",
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?
                ))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        if !idx.is_empty() {
            out.push_str(&format!("{}\n", idx.join("\n")));
        }
    }
    out
}

fn fresh() -> (DataDir, Connection) {
    let dir = DataDir::new("golden-schema");
    let mut conn = Connection::open(dir.db()).unwrap();
    db::apply_migrations(&mut conn).unwrap();
    (dir, conn)
}

#[test]
fn fresh_database_schema() {
    let (_dir, conn) = fresh();
    assert_snapshot!("schema_fresh", dump(&conn));
    assert_snapshot!(
        "schema_migrations_fresh",
        applied_migrations(&conn).join("\n")
    );
}

/// Each release's database ends up with exactly the fresh schema; the
/// matrix records what each upgrade applied and the backup it named.
#[tokio::test]
async fn upgraded_schemas_and_backup_names() {
    let (_fresh_dir, conn) = fresh();
    let expected = dump(&conn);
    let mut out = format!(
        "fresh schema sha256 {}\n\n",
        &norm::sha256(expected.as_bytes())[..16]
    );
    let stamp = regex::Regex::new(r"-\d{9,}(-\d+)?\.db$").unwrap();
    for f in fixtures() {
        let dir = DataDir::with_fixture(&f);
        let pool = db::build_pool(&dir.db());
        let report = db::migrate(&pool, &dir.backups())
            .await
            .unwrap_or_else(|e| panic!("{}: {e}", f.version));
        db::close(&pool).await;
        crate::common::assert_closed(&dir.db());

        let upgraded = dump(&open_ro(&dir.db()));
        assert_eq!(
            upgraded, expected,
            "{}: schema differs from a fresh database",
            f.version
        );
        let backup = report
            .backup
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| {
                stamp
                    .replace(&n.to_string_lossy(), "-<unix-seconds>.db")
                    .into_owned()
            })
            .unwrap_or_else(|| "none".into());
        out.push_str(&format!(
            "== {}\nhad: {}\napplied: {}\nbackup: {}\nschema sha256 {}\n\n",
            f.version,
            f.migrations().len(),
            report.applied.join(", "),
            backup,
            &norm::sha256(upgraded.as_bytes())[..16]
        ));
    }
    assert_snapshot!("schema_upgrade_matrix", out);
}

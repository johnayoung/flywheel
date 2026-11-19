//! A small SQLite file the hub keeps beside `state.json`, opened lazily:
//! created 0600, WAL, its schema checked by `PRAGMA user_version` and
//! made (or brought up) on first use. A hub whose file cannot be opened
//! keeps running and says why on each use.
//!
//! For data `state.json` must not hold: it is one document rewritten on
//! every edit, so anything with churn (leases) or many rows (reviews)
//! lives in one of these instead.

use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

/// Brings a file at `from` (0: new) up to the store's schema.
pub type Migrate = fn(&Connection, i64) -> rusqlite::Result<()>;

/// One lazily opened SQLite file.
#[derive(Debug)]
pub struct LazyDb {
    path: PathBuf,
    schema: i64,
    migrate: Migrate,
    conn: Mutex<Option<Connection>>,
}

impl LazyDb {
    pub fn new(path: PathBuf, schema: i64, migrate: Migrate) -> Self {
        Self {
            path,
            schema,
            migrate,
            conn: Mutex::new(None),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Run `f` on the connection, opening it first if need be.
    pub fn with<T>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Result<T, String> {
        let mut guard = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        if guard.is_none() {
            *guard = Some(open_store(&self.path, self.schema, self.migrate)?);
        }
        let conn = guard.as_ref().expect("opened above");
        f(conn).map_err(|e| format!("{}: {e}", self.path.display()))
    }

    /// Run `f` in one transaction.
    pub fn transaction<T>(
        &self,
        f: impl FnOnce(&rusqlite::Transaction<'_>) -> rusqlite::Result<T>,
    ) -> Result<T, String> {
        let mut guard = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
        if guard.is_none() {
            *guard = Some(open_store(&self.path, self.schema, self.migrate)?);
        }
        let conn = guard.as_mut().expect("opened above");
        let fail = |e: rusqlite::Error| format!("{}: {e}", self.path.display());
        let tx = conn.transaction().map_err(fail)?;
        let out = f(&tx).map_err(fail)?;
        tx.commit().map_err(fail)?;
        Ok(out)
    }
}

/// Open `path` (created 0600 when new), WAL, and bring it to `schema`
/// with `migrate`; refused when a newer hub wrote it.
pub fn open_store(path: &Path, schema: i64, migrate: Migrate) -> Result<Connection, String> {
    let fail = |e: rusqlite::Error| format!("{}: {e}", path.display());
    create_private(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let conn = Connection::open(path).map_err(fail)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(fail)?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(fail)?;
    let found: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(fail)?;
    if found > schema {
        return Err(format!(
            "{} was written by a newer hub (schema {found}, this one reads {schema})",
            path.display()
        ));
    }
    if found < schema {
        migrate(&conn, found).map_err(fail)?;
        conn.pragma_update(None, "user_version", schema)
            .map_err(fail)?;
    }
    Ok(conn)
}

/// Make `path`'s directory, and `path` itself mode 0600 when it is new
/// (before SQLite opens it, so it is never readable by others).
fn create_private(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make(conn: &Connection, from: i64) -> rusqlite::Result<()> {
        if from < 1 {
            conn.execute_batch("CREATE TABLE t (v INTEGER);")?;
        }
        Ok(())
    }

    #[test]
    fn a_store_is_made_private_and_reopened_without_migrating_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("x.db");
        let db = LazyDb::new(path.clone(), 1, make);
        db.with(|c| c.execute("INSERT INTO t VALUES (7)", []))
            .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        drop(db);
        let again = LazyDb::new(path.clone(), 1, make);
        let v: i64 = again
            .with(|c| c.query_row("SELECT v FROM t", [], |r| r.get(0)))
            .unwrap();
        assert_eq!(v, 7);
        drop(again);
        let older = LazyDb::new(path, 0, make);
        let refused = older.with(|c| c.query_row("SELECT 1", [], |r| r.get::<_, i64>(0)));
        assert!(refused.unwrap_err().contains("newer hub"));
    }
}

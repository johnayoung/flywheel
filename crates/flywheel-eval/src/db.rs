//! The two SQL backends the eval store runs on, behind one small seam.
//!
//! The eval tables use only portable SQL (`?N` placeholders rewritten to
//! `$N` for Postgres, `INSERT .. ON CONFLICT`, fixed-width text timestamps,
//! integer/real/text columns), so the store's queries are written once and
//! both backends run them. Concurrency is optimistic: every contended write
//! is an `UPDATE .. WHERE version = ?` whose row count says who won, so no
//! cross-statement transaction is needed except [`Db::batch`], which runs a
//! list of statements atomically.

use std::sync::OnceLock;

use flywheel_core::pg_tls::{SyncClient, off_runtime};
use flywheel_core::store_postgres as pg;
use flywheel_core::store_sqlite as lite;
use postgres::types::{ToSql, Type};
use regex::Regex;
use rusqlite::{Connection, TransactionBehavior};

use crate::store::EvalStoreError;

/// A bound parameter or a read column. Nulls are typed so Postgres can bind
/// them to the column's type.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Text(Option<String>),
    Int(Option<i64>),
    Real(Option<f64>),
}

impl Value {
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text(Some(s.into()))
    }

    pub fn opt_text(s: Option<impl Into<String>>) -> Self {
        Self::Text(s.map(Into::into))
    }

    pub fn int(i: impl Into<i64>) -> Self {
        Self::Int(Some(i.into()))
    }

    pub fn real(f: f64) -> Self {
        Self::Real(Some(f))
    }
}

/// One result row.
#[derive(Debug, Clone)]
pub struct Row(pub Vec<Value>);

impl Row {
    fn at(&self, i: usize) -> Result<&Value, EvalStoreError> {
        self.0
            .get(i)
            .ok_or_else(|| EvalStoreError::Corrupt(format!("row has no column {i}")))
    }

    pub fn opt_text(&self, i: usize) -> Result<Option<String>, EvalStoreError> {
        match self.at(i)? {
            Value::Text(v) => Ok(v.clone()),
            Value::Int(Some(v)) => Ok(Some(v.to_string())),
            Value::Int(None) | Value::Real(None) => Ok(None),
            Value::Real(Some(v)) => Ok(Some(v.to_string())),
        }
    }

    pub fn text(&self, i: usize) -> Result<String, EvalStoreError> {
        self.opt_text(i)?
            .ok_or_else(|| EvalStoreError::Corrupt(format!("column {i} is NULL")))
    }

    pub fn opt_int(&self, i: usize) -> Result<Option<i64>, EvalStoreError> {
        match self.at(i)? {
            Value::Int(v) => Ok(*v),
            Value::Real(v) => Ok(v.map(|f| f as i64)),
            Value::Text(None) => Ok(None),
            Value::Text(Some(s)) => s
                .parse()
                .map(Some)
                .map_err(|_| EvalStoreError::Corrupt(format!("column {i} is not an integer"))),
        }
    }

    pub fn int(&self, i: usize) -> Result<i64, EvalStoreError> {
        self.opt_int(i)?
            .ok_or_else(|| EvalStoreError::Corrupt(format!("column {i} is NULL")))
    }

    pub fn opt_real(&self, i: usize) -> Result<Option<f64>, EvalStoreError> {
        match self.at(i)? {
            Value::Real(v) => Ok(*v),
            Value::Int(v) => Ok(v.map(|i| i as f64)),
            Value::Text(None) => Ok(None),
            Value::Text(Some(s)) => s
                .parse()
                .map(Some)
                .map_err(|_| EvalStoreError::Corrupt(format!("column {i} is not a number"))),
        }
    }

    pub fn real(&self, i: usize) -> Result<f64, EvalStoreError> {
        Ok(self.opt_real(i)?.unwrap_or(0.0))
    }
}

/// A backend: run one statement, run a query, or run statements atomically.
pub trait Db: Send {
    fn execute(&mut self, sql: &str, params: &[Value]) -> Result<u64, EvalStoreError>;
    fn query(&mut self, sql: &str, params: &[Value]) -> Result<Vec<Row>, EvalStoreError>;
    fn batch(&mut self, statements: &[(String, Vec<Value>)]) -> Result<(), EvalStoreError>;
}

fn backend<E: std::error::Error + Send + Sync + 'static>(e: E) -> EvalStoreError {
    EvalStoreError::Backend(Box::new(e))
}

// --- SQLite ---------------------------------------------------------------------------

/// SQLite backend: one connection, a 5 s busy timeout, WAL.
pub struct SqliteDb {
    conn: Connection,
}

fn bind_sqlite(params: &[Value]) -> Vec<rusqlite::types::Value> {
    params
        .iter()
        .map(|p| match p {
            Value::Text(Some(s)) => rusqlite::types::Value::Text(s.clone()),
            Value::Int(Some(i)) => rusqlite::types::Value::Integer(*i),
            Value::Real(Some(f)) => rusqlite::types::Value::Real(*f),
            Value::Text(None) | Value::Int(None) | Value::Real(None) => {
                rusqlite::types::Value::Null
            }
        })
        .collect()
}

impl SqliteDb {
    /// Open the store file at `path`, creating the eval tables in a fresh
    /// store and refusing one at another eval schema version.
    pub fn open(path: &std::path::Path) -> Result<Self, EvalStoreError> {
        let conn = Connection::open(path).map_err(backend)?;
        Self::bootstrap(conn)
    }

    /// A private in-memory store (tests).
    pub fn in_memory() -> Result<Self, EvalStoreError> {
        Self::bootstrap(Connection::open_in_memory().map_err(backend)?)
    }

    fn bootstrap(mut conn: Connection) -> Result<Self, EvalStoreError> {
        conn.busy_timeout(std::time::Duration::from_millis(5000))
            .map_err(backend)?;
        if lite::recorded_version(&conn, crate::store::EVAL_VERSION_TABLE).map_err(backend)?
            == lite::Recorded::Version(Some(crate::store::CURRENT_EVAL_SCHEMA_VERSION))
        {
            return Ok(Self { conn });
        }
        let _ = conn.execute_batch("PRAGMA journal_mode = WAL;");
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(backend)?;
        match lite::recorded_version(&tx, crate::store::EVAL_VERSION_TABLE).map_err(backend)? {
            lite::Recorded::Missing => {
                if lite::any_table_exists(&tx, &crate::store::EVAL_TABLES).map_err(backend)? {
                    return Err(crate::store::schema_error(None));
                }
            }
            lite::Recorded::Version(Some(v)) if v == crate::store::CURRENT_EVAL_SCHEMA_VERSION => {}
            lite::Recorded::Version(observed) => return Err(crate::store::schema_error(observed)),
        }
        tx.execute_batch(crate::store::SCHEMA_SQL)
            .map_err(backend)?;
        tx.execute(
            "INSERT OR IGNORE INTO eval_schema_version (version, id) VALUES (?1, 1)",
            [i64::from(crate::store::CURRENT_EVAL_SCHEMA_VERSION)],
        )
        .map_err(backend)?;
        tx.commit().map_err(backend)?;
        Ok(Self { conn })
    }
}

fn read_sqlite(row: &rusqlite::Row<'_>, n: usize) -> rusqlite::Result<Row> {
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        out.push(match row.get_ref(i)? {
            rusqlite::types::ValueRef::Null => Value::Text(None),
            rusqlite::types::ValueRef::Integer(v) => Value::Int(Some(v)),
            rusqlite::types::ValueRef::Real(v) => Value::Real(Some(v)),
            rusqlite::types::ValueRef::Text(t) | rusqlite::types::ValueRef::Blob(t) => {
                Value::Text(Some(String::from_utf8_lossy(t).into_owned()))
            }
        });
    }
    Ok(Row(out))
}

impl Db for SqliteDb {
    fn execute(&mut self, sql: &str, params: &[Value]) -> Result<u64, EvalStoreError> {
        let bound = bind_sqlite(params);
        let n = self
            .conn
            .execute(sql, rusqlite::params_from_iter(bound))
            .map_err(backend)?;
        Ok(n as u64)
    }

    fn query(&mut self, sql: &str, params: &[Value]) -> Result<Vec<Row>, EvalStoreError> {
        let mut stmt = self.conn.prepare(sql).map_err(backend)?;
        let n = stmt.column_count();
        let rows = stmt
            .query_map(rusqlite::params_from_iter(bind_sqlite(params)), |row| {
                read_sqlite(row, n)
            })
            .map_err(backend)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(backend)
    }

    fn batch(&mut self, statements: &[(String, Vec<Value>)]) -> Result<(), EvalStoreError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(backend)?;
        for (sql, params) in statements {
            tx.execute(sql, rusqlite::params_from_iter(bind_sqlite(params)))
                .map_err(backend)?;
        }
        tx.commit().map_err(backend)
    }
}

// --- Postgres -------------------------------------------------------------------------

/// The advisory lock the eval bootstrap takes (distinct from the core and
/// orchestrator stores' key, so their bootstraps never wait on this one).
pub const EVAL_BOOTSTRAP_LOCK_KEY: i64 = 0x666C_7965;

/// Postgres backend: one connection pinned to `schema`.
pub struct PostgresDb {
    client: SyncClient,
}

fn placeholders(sql: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\?(\d+)").expect("static regex"))
        .replace_all(sql, "$$$1")
        .into_owned()
}

fn bind_pg(params: &[Value]) -> Vec<Box<dyn ToSql + Sync>> {
    params
        .iter()
        .map(|p| -> Box<dyn ToSql + Sync> {
            match p {
                Value::Text(v) => Box::new(v.clone()),
                Value::Int(v) => Box::new(*v),
                Value::Real(v) => Box::new(*v),
            }
        })
        .collect()
}

fn read_pg(row: &postgres::Row) -> Result<Row, EvalStoreError> {
    let mut out = Vec::with_capacity(row.len());
    for (i, column) in row.columns().iter().enumerate() {
        let ty = column.type_();
        out.push(if *ty == Type::INT8 {
            Value::Int(row.try_get::<_, Option<i64>>(i).map_err(backend)?)
        } else if *ty == Type::INT4 {
            Value::Int(
                row.try_get::<_, Option<i32>>(i)
                    .map_err(backend)?
                    .map(i64::from),
            )
        } else if *ty == Type::FLOAT8 {
            Value::Real(row.try_get::<_, Option<f64>>(i).map_err(backend)?)
        } else {
            Value::Text(row.try_get::<_, Option<String>>(i).map_err(backend)?)
        });
    }
    Ok(Row(out))
}

impl PostgresDb {
    /// Connect to `dsn` under `schema`, creating the eval tables in a fresh
    /// schema (under the eval bootstrap advisory lock) and refusing one at
    /// another eval schema version.
    pub fn connect(dsn: &str, schema: &str) -> Result<Self, EvalStoreError> {
        let schema = pg::validate_schema(schema).map_err(backend)?.to_owned();
        let (config, tls) = flywheel_core::pg_tls::configure(dsn).map_err(backend)?;
        off_runtime(|| {
            let mut client = SyncClient::new(config.connect(tls).map_err(backend)?);
            client
                .batch_execute(&format!("SET search_path TO \"{schema}\", public"))
                .map_err(backend)?;
            let mut tx = client.transaction().map_err(backend)?;
            tx.execute(
                "SELECT pg_advisory_xact_lock($1)",
                &[&EVAL_BOOTSTRAP_LOCK_KEY],
            )
            .map_err(backend)?;
            tx.batch_execute(&format!(
                "CREATE SCHEMA IF NOT EXISTS \"{schema}\"; SET search_path TO \"{schema}\", public"
            ))
            .map_err(backend)?;
            match pg::recorded_version(&mut tx, &schema, crate::store::EVAL_VERSION_TABLE)
                .map_err(backend)?
            {
                lite::Recorded::Missing => {
                    if pg::any_table_exists(&mut tx, &schema, &crate::store::EVAL_TABLES)
                        .map_err(backend)?
                    {
                        return Err(crate::store::schema_error(None));
                    }
                }
                lite::Recorded::Version(Some(v))
                    if v == crate::store::CURRENT_EVAL_SCHEMA_VERSION => {}
                lite::Recorded::Version(observed) => {
                    return Err(crate::store::schema_error(observed));
                }
            }
            tx.batch_execute(crate::store::SCHEMA_SQL_POSTGRES)
                .map_err(backend)?;
            tx.execute(
                "INSERT INTO eval_schema_version (version, id) VALUES ($1, 1) \
                 ON CONFLICT (id) DO NOTHING",
                &[&i64::from(crate::store::CURRENT_EVAL_SCHEMA_VERSION)],
            )
            .map_err(backend)?;
            tx.commit().map_err(backend)?;
            Ok(Self { client })
        })
    }
}

impl Db for PostgresDb {
    fn execute(&mut self, sql: &str, params: &[Value]) -> Result<u64, EvalStoreError> {
        let sql = placeholders(sql);
        let bound = bind_pg(params);
        off_runtime(|| {
            let refs: Vec<&(dyn ToSql + Sync)> = bound.iter().map(|b| b.as_ref()).collect();
            self.client.execute(&sql, &refs).map_err(backend)
        })
    }

    fn query(&mut self, sql: &str, params: &[Value]) -> Result<Vec<Row>, EvalStoreError> {
        let sql = placeholders(sql);
        let bound = bind_pg(params);
        off_runtime(|| {
            let refs: Vec<&(dyn ToSql + Sync)> = bound.iter().map(|b| b.as_ref()).collect();
            self.client
                .query(&sql, &refs)
                .map_err(backend)?
                .iter()
                .map(read_pg)
                .collect()
        })
    }

    fn batch(&mut self, statements: &[(String, Vec<Value>)]) -> Result<(), EvalStoreError> {
        off_runtime(|| {
            let mut tx = self.client.transaction().map_err(backend)?;
            for (sql, params) in statements {
                let bound = bind_pg(params);
                let refs: Vec<&(dyn ToSql + Sync)> = bound.iter().map(|b| b.as_ref()).collect();
                tx.execute(&placeholders(sql), &refs).map_err(backend)?;
            }
            tx.commit().map_err(backend)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_become_numbered_dollars() {
        assert_eq!(
            placeholders("SELECT a FROM t WHERE x = ?1 AND y = ?12"),
            "SELECT a FROM t WHERE x = $1 AND y = $12"
        );
    }
}

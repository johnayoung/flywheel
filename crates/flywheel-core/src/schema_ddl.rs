//! Pure helpers over the canonical DDL text the stores embed.
//!
//! The in-place schema upgrade (`fw db upgrade`) re-creates or re-keys
//! tables from the same `CREATE TABLE` statements a fresh store bootstraps
//! with, so the upgraded layout can never drift from the fresh one. These
//! helpers slice those statements out of a DDL script. They rely on the
//! layout every flywheel DDL script follows: one
//! `CREATE TABLE IF NOT EXISTS <name> (` header per table, one column or
//! table constraint per line, and the closing `);` alone at column zero.
//! No IO here; the concrete stores run the statements.

/// The `CREATE TABLE IF NOT EXISTS <table> (...);` statement for `table` in
/// `ddl` (header through the closing `);`), or `None` when the script does
/// not define it.
pub fn create_table_statement<'a>(ddl: &'a str, table: &str) -> Option<&'a str> {
    let header = format!("CREATE TABLE IF NOT EXISTS {table} (");
    let start = ddl.find(&header)?;
    let rest = &ddl[start..];
    let end = rest.find("\n);")?;
    Some(&rest[..end + "\n);".len()])
}

/// The same statement retargeted at `new_name` without `IF NOT EXISTS` (the
/// shadow table a rebuild copies rows into).
pub fn create_table_as(ddl: &str, table: &str, new_name: &str) -> Option<String> {
    let statement = create_table_statement(ddl, table)?;
    let header = format!("CREATE TABLE IF NOT EXISTS {table} (");
    Some(statement.replacen(&header, &format!("CREATE TABLE {new_name} ("), 1))
}

/// The table-level constraint clauses of `table` (`PRIMARY KEY (...)`,
/// `UNIQUE (...)`, `FOREIGN KEY (...) REFERENCES ...`), trailing comma
/// stripped, in declaration order. Column-level constraints (an inline
/// `id ... PRIMARY KEY`) are not returned.
pub fn table_constraints(ddl: &str, table: &str) -> Vec<String> {
    let Some(statement) = create_table_statement(ddl, table) else {
        return Vec::new();
    };
    statement
        .lines()
        .skip(1)
        .map(str::trim)
        .filter(|line| {
            line.starts_with("PRIMARY KEY")
                || line.starts_with("UNIQUE")
                || line.starts_with("FOREIGN KEY")
        })
        .map(|line| line.trim_end_matches(',').to_owned())
        .collect()
}

/// Every column of `table` as `(name, definition)`, in declaration order;
/// the definition is the declaring line minus its trailing comma (so
/// `ALTER TABLE t ADD COLUMN <definition>` re-creates it).
pub fn column_definitions(ddl: &str, table: &str) -> Vec<(String, String)> {
    let Some(statement) = create_table_statement(ddl, table) else {
        return Vec::new();
    };
    statement
        .lines()
        .skip(1)
        .map(str::trim)
        .filter(|line| {
            !line.is_empty()
                && !line.starts_with("--")
                && !line.starts_with(')')
                && !line.starts_with("PRIMARY KEY")
                && !line.starts_with("UNIQUE")
                && !line.starts_with("FOREIGN KEY")
                && !line.starts_with("CHECK")
        })
        .filter_map(|line| {
            let name = line.split_whitespace().next()?;
            Some((name.to_owned(), line.trim_end_matches(',').to_owned()))
        })
        .collect()
}

/// Every column name declared in `table`'s statement, in order.
pub fn declared_columns(ddl: &str, table: &str) -> Vec<String> {
    column_definitions(ddl, table)
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

/// `ddl` with every `PRAGMA ...;` line removed: the statements that may run
/// inside a transaction (SQLite refuses `journal_mode` changes in one).
pub fn without_pragmas(ddl: &str) -> String {
    ddl.lines()
        .filter(|line| !line.trim_start().starts_with("PRAGMA"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `value` as a single-quoted SQL string literal (quotes doubled). Only for
/// DDL positions that cannot take a bound parameter (a column `DEFAULT`).
pub fn sql_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DDL: &str = "
-- header
CREATE TABLE IF NOT EXISTS parent (
  project_id TEXT NOT NULL,
  id         TEXT NOT NULL,
  -- a comment (with parens);
  note       TEXT,
  PRIMARY KEY (project_id, id)
);
CREATE INDEX IF NOT EXISTS idx_parent ON parent(project_id, note);

CREATE TABLE IF NOT EXISTS child (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  project_id TEXT NOT NULL,
  parent_id  TEXT NOT NULL,
  seq        INTEGER NOT NULL,
  FOREIGN KEY (project_id, parent_id) REFERENCES parent(project_id, id),
  UNIQUE (project_id, parent_id, seq)
);
PRAGMA foreign_keys = ON;
";

    #[test]
    fn extracts_one_statement_per_table() {
        let parent = create_table_statement(DDL, "parent").unwrap();
        assert!(parent.starts_with("CREATE TABLE IF NOT EXISTS parent ("));
        assert!(parent.ends_with("\n);"));
        assert!(!parent.contains("idx_parent"));
        assert!(create_table_statement(DDL, "missing").is_none());
        // A table whose name prefixes another's is not confused with it.
        assert!(create_table_statement(DDL, "chil").is_none());
    }

    #[test]
    fn retargets_and_lists_constraints_and_columns() {
        let shadow = create_table_as(DDL, "child", "child__partitioned").unwrap();
        assert!(shadow.starts_with("CREATE TABLE child__partitioned ("));
        assert_eq!(
            table_constraints(DDL, "child"),
            vec![
                "FOREIGN KEY (project_id, parent_id) REFERENCES parent(project_id, id)",
                "UNIQUE (project_id, parent_id, seq)",
            ]
        );
        assert_eq!(
            table_constraints(DDL, "parent"),
            vec!["PRIMARY KEY (project_id, id)"]
        );
        assert_eq!(
            declared_columns(DDL, "parent"),
            vec!["project_id", "id", "note"]
        );
        assert_eq!(
            column_definitions(DDL, "child")[0],
            (
                "id".to_owned(),
                "id         INTEGER PRIMARY KEY AUTOINCREMENT".to_owned()
            )
        );
    }

    #[test]
    fn strips_pragmas_and_quotes_literals() {
        assert!(!without_pragmas(DDL).contains("PRAGMA"));
        assert!(without_pragmas(DDL).contains("CREATE TABLE IF NOT EXISTS child"));
        assert_eq!(sql_literal("it's"), "'it''s'");
    }
}

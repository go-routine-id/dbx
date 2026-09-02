//! Structural comparison between two schemas (e.g. dev vs prod).
//!
//! Pure over [`CollectionMeta`], so it is fully testable without a database
//! and works for any driver. The output is both human-readable (what differs)
//! and executable (the DDL that would bring `target` in line with `source`).

use crate::driver::{CollectionMeta, ColumnMeta};

/// One structural difference, phrased as "what `target` is missing or has
/// differently compared to `source`".
#[derive(Clone, Debug, PartialEq)]
pub enum Difference {
    /// Present in source, absent in target.
    MissingTable { table: String },
    /// Present in target, absent in source.
    ExtraTable { table: String },
    MissingColumn {
        table: String,
        column: String,
        data_type: String,
    },
    ExtraColumn { table: String, column: String },
    TypeChanged {
        table: String,
        column: String,
        from: String,
        to: String,
    },
    NullabilityChanged {
        table: String,
        column: String,
        /// Nullability in the source (the side being matched).
        source_nullable: bool,
    },
}

impl Difference {
    /// The table this difference belongs to, for grouping in the UI.
    pub fn table(&self) -> &str {
        match self {
            Difference::MissingTable { table }
            | Difference::ExtraTable { table }
            | Difference::MissingColumn { table, .. }
            | Difference::ExtraColumn { table, .. }
            | Difference::TypeChanged { table, .. }
            | Difference::NullabilityChanged { table, .. } => table,
        }
    }

    /// One-line description as shown in the diff overlay.
    pub fn describe(&self) -> String {
        match self {
            Difference::MissingTable { table } => format!("- table {table} is missing"),
            Difference::ExtraTable { table } => format!("+ table {table} exists only here"),
            Difference::MissingColumn { table, column, data_type } => {
                format!("- {table}.{column} is missing ({data_type})")
            }
            Difference::ExtraColumn { table, column } => {
                format!("+ {table}.{column} exists only here")
            }
            Difference::TypeChanged { table, column, from, to } => {
                format!("~ {table}.{column} type {to} -> {from}")
            }
            Difference::NullabilityChanged { table, column, source_nullable } => {
                let want = if *source_nullable { "NULL" } else { "NOT NULL" };
                format!("~ {table}.{column} should be {want}")
            }
        }
    }
}

fn column<'a>(meta: &'a CollectionMeta, name: &str) -> Option<&'a ColumnMeta> {
    meta.columns.iter().find(|c| c.name == name)
}

/// Compare two schemas. Differences are phrased as changes `target` would need
/// to match `source`, and are returned in a stable order (by table, then by
/// the source's column order) so the output does not churn between runs.
pub fn diff_schemas(source: &[CollectionMeta], target: &[CollectionMeta]) -> Vec<Difference> {
    let find = |list: &[CollectionMeta], name: &str| -> Option<CollectionMeta> {
        list.iter().find(|m| m.reference.name == name).cloned()
    };

    let mut names: Vec<String> = source
        .iter()
        .chain(target.iter())
        .map(|m| m.reference.name.clone())
        .collect();
    names.sort();
    names.dedup();

    let mut out = Vec::new();
    for name in names {
        match (find(source, &name), find(target, &name)) {
            (Some(_), None) => out.push(Difference::MissingTable { table: name }),
            (None, Some(_)) => out.push(Difference::ExtraTable { table: name }),
            (Some(src), Some(tgt)) => {
                // Columns the target lacks, plus per-column drift.
                for sc in &src.columns {
                    match column(&tgt, &sc.name) {
                        None => out.push(Difference::MissingColumn {
                            table: name.clone(),
                            column: sc.name.clone(),
                            data_type: sc.data_type.clone(),
                        }),
                        Some(tc) => {
                            // Type names are compared case-insensitively:
                            // engines echo them back inconsistently.
                            if !sc.data_type.eq_ignore_ascii_case(&tc.data_type) {
                                out.push(Difference::TypeChanged {
                                    table: name.clone(),
                                    column: sc.name.clone(),
                                    from: sc.data_type.clone(),
                                    to: tc.data_type.clone(),
                                });
                            }
                            if sc.is_nullable != tc.is_nullable {
                                out.push(Difference::NullabilityChanged {
                                    table: name.clone(),
                                    column: sc.name.clone(),
                                    source_nullable: sc.is_nullable,
                                });
                            }
                        }
                    }
                }
                // Columns only the target has.
                for tc in &tgt.columns {
                    if column(&src, &tc.name).is_none() {
                        out.push(Difference::ExtraColumn {
                            table: name.clone(),
                            column: tc.name.clone(),
                        });
                    }
                }
            }
            (None, None) => {}
        }
    }
    out
}

/// DDL that would bring `target` in line with `source`.
///
/// Destructive steps (dropping a table or column that exists only in the
/// target) are emitted as **comments**: losing data must be a deliberate act,
/// not something a generated script does on your behalf.
pub fn migration_sql(diffs: &[Difference], namespace: &str, driver_name: &str) -> String {
    let q = |ident: &str| crate::sql::quote_ident(ident, driver_name);
    let mut out = String::new();
    for d in diffs {
        let t = format!("{}.{}", q(namespace), q(d.table()));
        match d {
            Difference::MissingTable { table } => {
                out.push_str(&format!(
                    "-- TODO: create table {table} (structure not generated - use its DDL)\n"
                ));
            }
            Difference::ExtraTable { table } => {
                out.push_str(&format!(
                    "-- DROP TABLE {}.{}; -- destructive: review before running\n",
                    q(namespace),
                    q(table)
                ));
            }
            Difference::MissingColumn { column, data_type, .. } => {
                out.push_str(&format!(
                    "ALTER TABLE {t} ADD COLUMN {} {data_type};\n",
                    q(column)
                ));
            }
            Difference::ExtraColumn { column, .. } => {
                out.push_str(&format!(
                    "-- ALTER TABLE {t} DROP COLUMN {}; -- destructive: review before running\n",
                    q(column)
                ));
            }
            Difference::TypeChanged { column, from, .. } => {
                out.push_str(&format!(
                    "ALTER TABLE {t} ALTER COLUMN {} TYPE {from};\n",
                    q(column)
                ));
            }
            Difference::NullabilityChanged { column, source_nullable, .. } => {
                let action = if *source_nullable {
                    "DROP NOT NULL"
                } else {
                    "SET NOT NULL"
                };
                out.push_str(&format!(
                    "ALTER TABLE {t} ALTER COLUMN {} {action};\n",
                    q(column)
                ));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::{CollectionRef, Namespace};

    fn col(name: &str, ty: &str, nullable: bool) -> ColumnMeta {
        ColumnMeta {
            name: name.to_string(),
            data_type: ty.to_string(),
            is_nullable: nullable,
            is_primary_key: false,
            is_unique: false,
            is_foreign_key: false,
            extra: None,
        }
    }

    fn meta(name: &str, columns: Vec<ColumnMeta>) -> CollectionMeta {
        CollectionMeta {
            reference: CollectionRef {
                namespace: Namespace("public".to_string()),
                name: name.to_string(),
            },
            columns,
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
        }
    }

    /// End-to-end over two real SQLite files, so the diff is proven against
    /// metadata a driver actually produced rather than hand-built structs.
    #[tokio::test]
    async fn test_diff_over_two_real_sqlite_databases() {
        use crate::config::{ConnectionConfig, DriverType};
        use crate::driver::Driver;

        async fn seed(path: &std::path::Path, stmts: &[&'static str]) {
            let opts = sqlx::sqlite::SqliteConnectOptions::new()
                .filename(path)
                .create_if_missing(true);
            let pool = sqlx::sqlite::SqlitePool::connect_with(opts).await.unwrap();
            for st in stmts {
                sqlx::query(*st).execute(&pool).await.unwrap();
            }
            pool.close().await;
        }
        fn cfg(path: &std::path::Path) -> ConnectionConfig {
            ConnectionConfig {
                name: "x".into(),
                driver: DriverType::Sqlite,
                host: String::new(),
                port: None,
                user: None,
                password: None,
                database: Some(path.to_string_lossy().into_owned()),
                socket: None,
                ssl: false,
                ssl_mode: None,
            }
        }
        async fn schema(drv: &dyn Driver) -> Vec<CollectionMeta> {
            let ns = Namespace("main".to_string());
            let mut out = Vec::new();
            for t in drv.collections(&ns).await.unwrap() {
                let cref = CollectionRef { namespace: ns.clone(), name: t.name };
                out.push(drv.collection_meta(&cref).await.unwrap());
            }
            out
        }

        let dir = std::env::temp_dir();
        let uniq = std::process::id();
        let (dev, prod) = (
            dir.join(format!("dbx-diff-dev-{uniq}.db")),
            dir.join(format!("dbx-diff-prod-{uniq}.db")),
        );
        let _ = std::fs::remove_file(&dev);
        let _ = std::fs::remove_file(&prod);

        seed(
            &dev,
            &[
                "CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT NOT NULL, email TEXT)",
                "CREATE TABLE audit(id INTEGER PRIMARY KEY)",
            ],
        )
        .await;
        seed(
            &prod,
            &["CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT, legacy TEXT)"],
        )
        .await;

        let d1 = crate::driver::sqlite::SqliteDriver::connect(&cfg(&dev)).await.unwrap();
        let d2 = crate::driver::sqlite::SqliteDriver::connect(&cfg(&prod)).await.unwrap();
        let diffs = diff_schemas(&schema(&d1).await, &schema(&d2).await);

        // prod lacks the whole audit table and the email column...
        assert!(diffs.contains(&Difference::MissingTable { table: "audit".into() }));
        assert!(diffs.iter().any(|d| matches!(
            d,
            Difference::MissingColumn { table, column, .. } if table == "users" && column == "email"
        )));
        // ...has a column dev doesn't...
        assert!(diffs.contains(&Difference::ExtraColumn {
            table: "users".into(),
            column: "legacy".into(),
        }));
        // ...and `name` drifted to nullable.
        assert!(diffs.contains(&Difference::NullabilityChanged {
            table: "users".into(),
            column: "name".into(),
            source_nullable: false,
        }));

        // The generated migration adds, but never silently destroys.
        let sql = migration_sql(&diffs, "main", "SQLite");
        assert!(sql.contains("ADD COLUMN"), "{sql}");
        for line in sql.lines().filter(|l| l.contains("DROP")) {
            assert!(line.trim_start().starts_with("--"), "{line}");
        }

        let _ = std::fs::remove_file(&dev);
        let _ = std::fs::remove_file(&prod);
    }

    #[test]
    fn test_identical_schemas_have_no_differences() {
        let a = vec![meta("users", vec![col("id", "int", false)])];
        let b = vec![meta("users", vec![col("id", "int", false)])];
        assert!(diff_schemas(&a, &b).is_empty());
    }

    #[test]
    fn test_detects_table_and_column_drift() {
        let source = vec![
            meta("users", vec![col("id", "int", false), col("email", "text", true)]),
            meta("audit", vec![col("id", "int", false)]),
        ];
        let target = vec![
            meta("users", vec![col("id", "int", false), col("legacy", "text", true)]),
            meta("temp_junk", vec![col("id", "int", false)]),
        ];
        let diffs = diff_schemas(&source, &target);

        assert!(diffs.contains(&Difference::MissingTable { table: "audit".into() }));
        assert!(diffs.contains(&Difference::ExtraTable { table: "temp_junk".into() }));
        assert!(diffs.contains(&Difference::MissingColumn {
            table: "users".into(),
            column: "email".into(),
            data_type: "text".into(),
        }));
        assert!(diffs.contains(&Difference::ExtraColumn {
            table: "users".into(),
            column: "legacy".into(),
        }));
    }

    #[test]
    fn test_type_comparison_ignores_case_but_catches_real_changes() {
        let source = vec![meta("t", vec![col("a", "INTEGER", false)])];
        // Same type, different casing -> not a difference.
        let same = vec![meta("t", vec![col("a", "integer", false)])];
        assert!(diff_schemas(&source, &same).is_empty());

        let changed = vec![meta("t", vec![col("a", "text", false)])];
        assert_eq!(
            diff_schemas(&source, &changed),
            vec![Difference::TypeChanged {
                table: "t".into(),
                column: "a".into(),
                from: "INTEGER".into(),
                to: "text".into(),
            }]
        );
    }

    #[test]
    fn test_detects_nullability_drift() {
        let source = vec![meta("t", vec![col("a", "int", false)])];
        let target = vec![meta("t", vec![col("a", "int", true)])];
        assert_eq!(
            diff_schemas(&source, &target),
            vec![Difference::NullabilityChanged {
                table: "t".into(),
                column: "a".into(),
                source_nullable: false,
            }]
        );
    }

    #[test]
    fn test_migration_keeps_destructive_steps_commented_out() {
        let diffs = vec![
            Difference::MissingColumn {
                table: "users".into(),
                column: "email".into(),
                data_type: "text".into(),
            },
            Difference::ExtraColumn {
                table: "users".into(),
                column: "legacy".into(),
            },
            Difference::ExtraTable { table: "junk".into() },
        ];
        let sql = migration_sql(&diffs, "public", "PostgreSQL 15.3");

        // Additive change is runnable...
        assert!(sql.contains(r#"ALTER TABLE "public"."users" ADD COLUMN "email" text;"#), "{sql}");
        // ...but nothing that destroys data is.
        for line in sql.lines().filter(|l| l.contains("DROP")) {
            assert!(
                line.trim_start().starts_with("--"),
                "destructive step must be commented: {line}"
            );
        }
    }

    #[test]
    fn test_migration_quotes_per_dialect() {
        let diffs = vec![Difference::MissingColumn {
            table: "users".into(),
            column: "email".into(),
            data_type: "text".into(),
        }];
        assert!(migration_sql(&diffs, "shop", "MySQL 8.0").contains("`shop`.`users`"));
        assert!(migration_sql(&diffs, "shop", "PostgreSQL 15").contains(r#""shop"."users""#));
    }
}

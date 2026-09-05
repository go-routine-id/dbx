//! Generic SQL building shared by every driver.
//!
//! Split out of `app.rs`: these are pure string functions with no UI or
//! connection state, so keeping them beside the event loop only made that
//! file harder to read.

/// Sentinel written into an edit buffer to mean "set this cell to SQL NULL",
/// as opposed to the literal text "NULL".
pub const NULL_SENTINEL: &str = "__DBX_NULL__";

// ---------------------------------------------------------------------------
// Generic SQL building helpers (multi-driver safe)
// ---------------------------------------------------------------------------
//
// These are intentionally **driver-agnostic** in their inputs: they take raw
// strings and build SQL that works against any backend. The only driver
// specifics that leak through are:
//   1. Identifier quoting style (PG uses `"..."`, MySQL uses `` `...` ``).
//   2. The `LIMIT 1` suffix that MySQL requires on a `DELETE` statement
//      without an implied unique key (PostgreSQL rejects it).
//   3. NULL literal rendering — same `NULL` keyword on every SQL backend,
//      no quoting.
//
// All other concerns (string value escaping, NULL sentinel, skip columns,
// dialect-agnostic UPDATE/INSERT syntax) are uniform. The driver crate
// remains the source of truth for the connection dialect — we sniff
// `DriverInfo::name` here purely for the two style differences above.

/// Quoting style for a SQL dialect. Currently we only need to distinguish
/// PostgreSQL (double-quote identifiers) from MySQL/SQLite (backtick) and
/// SQL Server (square brackets). When SQL Server / SQLite land, extend the
/// match in `quote_ident_for` — everything downstream stays the same.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuoteStyle {
    /// PostgreSQL: `"identifier"` (case-sensitive).
    Double,
    /// MySQL / MariaDB: `` `identifier` `` (case-insensitive on default
    /// lower_case_table_names). Also what SQLite accepts for compatibility.
    Backtick,
    /// SQL Server: `[identifier]` or `"identifier"` (the latter is
    /// non-standard and conflicts with string literals in some contexts).
    Bracket,
    /// redis: no quoting at all — Redis has no identifier syntax (key
    /// prefixes are plain strings); the identifier is emitted unchanged.
    Raw,
}

/// Map a driver-info name to its identifier quoting style. Defaults to
/// double-quote (PG) for any unknown driver — safest for a SQL-syntax
/// reference, and the connection itself will reject the SQL if the dialect
/// is wildly different.
pub fn quote_style_for(driver_name: &str) -> QuoteStyle {
    let lower = driver_name.to_lowercase();
    if lower.contains("postgres") || lower.contains("pg") {
        QuoteStyle::Double
    } else if lower.contains("mysql") || lower.contains("maria") {
        QuoteStyle::Backtick
    } else if lower.contains("sql server") || lower.contains("mssql") {
        QuoteStyle::Bracket
    } else if lower.contains("sqlite") {
        // SQLite accepts both. Backtick is the most ergonomic for
        // round-tripped queries.
        QuoteStyle::Backtick
    } else if lower.contains("redis") {
        // redis: keys aren't SQL identifiers; quoting them would corrupt
        // any text the user pastes into the command console.
        QuoteStyle::Raw
    } else {
        QuoteStyle::Double
    }
}

/// Quote a SQL identifier (table / column / schema name) according to the
/// given style. Doubles up the inner quote character so the identifier
/// itself can contain the quote without breaking out — same defensive
/// pattern that PostgreSQL/MySQL driver crates already apply internally.
pub fn quote_ident_with(ident: &str, style: QuoteStyle) -> String {
    match style {
        QuoteStyle::Double => format!("\"{}\"", ident.replace('"', "\"\"")),
        QuoteStyle::Backtick => format!("`{}`", ident.replace('`', "``")),
        QuoteStyle::Bracket => format!("[{}]", ident.replace(']', "]]")),
        QuoteStyle::Raw => ident.to_string(), // redis
    }
}

/// Convenience wrapper that pulls the style from a `DriverInfo`-style name.
/// Kept as a single entry point so call sites never have to think about the
/// underlying style enum.
pub fn quote_ident(ident: &str, driver_name: &str) -> String {
    quote_ident_with(ident, quote_style_for(driver_name))
}

/// MySQL requires `LIMIT 1` at the end of a single-row DELETE (and UPDATE)
/// when the WHERE clause doesn't target a unique key. PostgreSQL rejects
/// the `LIMIT` clause in DELETE/UPDATE. Other dialects vary. This returns
/// the dialect-appropriate suffix to append after the WHERE clause.
pub fn single_row_suffix(driver_name: &str) -> &'static str {
    let lower = driver_name.to_lowercase();
    if lower.contains("mysql") || lower.contains("maria") {
        " LIMIT 1"
    } else {
        // PostgreSQL, SQL Server, SQLite: the WHERE clause is enough.
        ""
    }
}

/// Escape a user-supplied value for safe inclusion as a SQL string literal.
/// Doubles single quotes (the standard SQL escape). `None` (skip) is
/// handled by callers before reaching here. The empty string becomes
/// `''` which is a valid empty literal on every backend.
pub fn escape_string_literal(s: &str) -> String {
    s.replace('\'', "''")
}

/// Render a single `Value` as the right-hand side of `col = ...` or as a
/// WHERE-clause comparison value. NULL is the keyword `NULL` (never
/// quoted, never escaped — this is the standard SQL semantics shared by
/// every backend). All other values are emitted as quoted string
/// literals; the server will coerce them to the column's actual type.
/// This matches what every DB GUI does — it's the only way to stay
/// driver-agnostic without dragging in type-aware formatting for INT,
/// BOOL, TIMESTAMP, NUMERIC, etc. that differs subtly per dialect.
pub fn render_value_sql(val: &crate::driver::Value) -> String {
    match val {
        crate::driver::Value::Null => "NULL".to_string(),
        other => format!("'{}'", escape_string_literal(&other.display_str())),
    }
}

/// Render the user-edited buffer that lives in a modal (`CellEditModalState`
/// or `InsertRowModalState`). The `__DBX_NULL__` sentinel from `Ctrl+N` is
/// translated to the bare `NULL` keyword; everything else is quoted. Empty
/// string is intentionally rendered as `''` (a valid empty literal) so
/// the user can blank out a non-nullable text column when desired.
pub fn render_buffer_sql(buf: &str) -> String {
    if buf == NULL_SENTINEL {
        "NULL".to_string()
    } else {
        format!("'{}'", escape_string_literal(buf))
    }
}

/// Build a WHERE clause that targets a single row from a table page,
/// preferring primary-key columns (the safe, narrow target) and falling
/// back to a full-row match when no PK is defined. Returns the WHERE
/// fragment without the leading `WHERE ` keyword. Returns `None` when
/// no columns could be matched at all (e.g. zero-column table) so callers
/// can bail out with a toast instead of running a destructive statement
/// with an always-true WHERE.
pub fn build_where_for_row(
    columns: &[String],
    row: &crate::driver::Record,
    pk_cols: &[String],
    driver_name: &str,
) -> Option<String> {
    let mut where_clauses: Vec<String> = Vec::new();

    if !pk_cols.is_empty() {
        for pk in pk_cols {
            if let Some(pos) = columns.iter().position(|c| c == pk)
                && let Some(val) = row.values.get(pos)
            {
                let q_col = quote_ident(pk, driver_name);
                if matches!(val, crate::driver::Value::Null) {
                    where_clauses.push(format!("{q_col} IS NULL"));
                } else {
                    where_clauses.push(format!("{q_col} = {}", render_value_sql(val)));
                }
            }
        }
    } else {
        // No PK: match on every column. Less safe (could match multiple
        // rows if duplicates exist) but the only way to target a row in
        // tables without a defined primary key. The dialect-specific
        // `LIMIT 1` suffix appended by the caller narrows the blast radius
        // on backends that allow it.
        for (i, c) in columns.iter().enumerate() {
            if let Some(val) = row.values.get(i) {
                let q_col = quote_ident(c, driver_name);
                if matches!(val, crate::driver::Value::Null) {
                    where_clauses.push(format!("{q_col} IS NULL"));
                } else {
                    where_clauses.push(format!("{q_col} = {}", render_value_sql(val)));
                }
            }
        }
    }

    if where_clauses.is_empty() {
        None
    } else {
        Some(where_clauses.join(" AND "))
    }
}

/// Build a generic `INSERT INTO ns.tbl (col, col) VALUES (val, val)` from
/// a list of `(column_name, Option<buffer>)` pairs. `None` buffers are
/// skipped — the column is omitted from both the column list and the
/// values list, so the server applies the column's DEFAULT (or NULL when
/// nullable and no default, or rejects the row when NOT NULL with no
/// default). `Some(NULL_SENTINEL)` is rendered as the bare `NULL` keyword.
/// The output syntax is the standard SQL form that PostgreSQL, MySQL,
/// SQL Server, and SQLite all accept. Driver differences (PG `RETURNING`,
/// MySQL `LAST_INSERT_ID()`, etc.) are intentionally **not** baked in —
/// the caller can ignore rows_affected if it needs richer feedback.
pub fn build_insert_sql(
    cref: &crate::driver::CollectionRef,
    fields: &[(String, Option<String>)],
    driver_name: &str,
) -> Option<String> {
    // Strip out the skipped (None) columns. Order is preserved as supplied.
    let provided: Vec<(&String, &String)> = fields
        .iter()
        .filter_map(|(name, buf)| buf.as_ref().map(|b| (name, b)))
        .collect();
    if provided.is_empty() {
        return None;
    }
    let q_ns = quote_ident(&cref.namespace.0, driver_name);
    let q_tbl = quote_ident(&cref.name, driver_name);
    let col_list = provided
        .iter()
        .map(|(name, _)| quote_ident(name, driver_name))
        .collect::<Vec<_>>()
        .join(", ");
    let value_list = provided
        .iter()
        .map(|(_, buf)| render_buffer_sql(buf))
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "INSERT INTO {q_ns}.{q_tbl} ({col_list}) VALUES ({value_list});"
    ))
}

/// Build a single-row `INSERT` for copy-to-clipboard (row-as-INSERT), using
/// the dialect-aware identifier quoting + value rendering. Unlike
/// `Exporter::format_sql_insert` (which hardcodes backticks for the export
/// file), this is safe on PostgreSQL too.
pub fn build_insert_row_sql(
    table: &str,
    columns: &[String],
    row: &crate::driver::Record,
    driver_name: &str,
) -> String {
    let q_tbl = quote_ident(table, driver_name);
    let col_list = columns
        .iter()
        .map(|c| quote_ident(c, driver_name))
        .collect::<Vec<_>>()
        .join(", ");
    let value_list = row
        .values
        .iter()
        .map(render_value_sql)
        .collect::<Vec<_>>()
        .join(", ");
    format!("INSERT INTO {q_tbl} ({col_list}) VALUES ({value_list});")
}



/// Build an INSERT where every value is a quoted string literal — used by
/// CSV import so a cell whose text happens to equal the `__DBX_NULL__`
/// sentinel is inserted as that literal, never as SQL NULL.
pub fn build_insert_literal_sql(
    table: &str,
    columns: &[String],
    values: &[String],
    driver_name: &str,
) -> String {
    let q_tbl = quote_ident(table, driver_name);
    let col_list = columns
        .iter()
        .map(|c| quote_ident(c, driver_name))
        .collect::<Vec<_>>()
        .join(", ");
    let val_list = values
        .iter()
        .map(|v| format!("'{}'", escape_string_literal(v)))
        .collect::<Vec<_>>()
        .join(", ");
    format!("INSERT INTO {q_tbl} ({col_list}) VALUES ({val_list});")
}


/// Build ALTER TABLE statements from the schema-edit operations, using the
/// dialect-aware identifier quoting.
pub fn generate_alter_sql(
    collection: &crate::driver::CollectionRef,
    drop_cols: &[String],
    add_cols: &[(String, String)],
    type_changes: &[(String, String)],
    rename_table: Option<&str>,
    driver_name: &str,
) -> Option<String> {
    let q = quote_ident(&collection.namespace.0, driver_name);
    let t = quote_ident(&collection.name, driver_name);
    let mut stmts = Vec::new();
    for col in drop_cols {
        stmts.push(format!(
            "ALTER TABLE {q}.{t} DROP COLUMN {};",
            quote_ident(col, driver_name)
        ));
    }
    for (name, ty) in add_cols {
        stmts.push(format!(
            "ALTER TABLE {q}.{t} ADD COLUMN {} {ty};",
            quote_ident(name, driver_name)
        ));
    }
    for (col, new_type) in type_changes {
        // Dialect-aware type change: MySQL uses MODIFY COLUMN, PG uses
        // ALTER COLUMN ... TYPE.
        if driver_name.to_lowercase().contains("mysql")
            || driver_name.to_lowercase().contains("maria")
        {
            stmts.push(format!(
                "ALTER TABLE {q}.{t} MODIFY COLUMN {} {new_type};",
                quote_ident(col, driver_name)
            ));
        } else {
            stmts.push(format!(
                "ALTER TABLE {q}.{t} ALTER COLUMN {} TYPE {new_type};",
                quote_ident(col, driver_name)
            ));
        }
    }
    // Rename LAST so the earlier statements still reference the old name.
    if let Some(new) = rename_table {
        stmts.push(format!(
            "ALTER TABLE {q}.{t} RENAME TO {};",
            quote_ident(new, driver_name)
        ));
    }
    if stmts.is_empty() {
        None
    } else {
        Some(stmts.join("\n"))
    }
}

/// Build a CREATE statement for a new object. Uses simple templates — the
/// SQL-confirm modal lets the user refine before executing.
pub fn generate_create_sql(
    ns: &crate::driver::Namespace,
    kind: crate::ui::screens::explorer::CreateKind,
    name: &str,
    driver_name: &str,
) -> Option<String> {
    let mysql = driver_name.to_lowercase().contains("mysql")
        || driver_name.to_lowercase().contains("maria");
    let qn = quote_ident(name, driver_name);
    let ns_q = quote_ident(&ns.0, driver_name);
    match kind {
        crate::ui::screens::explorer::CreateKind::Schema => Some(format!("CREATE SCHEMA {qn};")),
        crate::ui::screens::explorer::CreateKind::Table => {
            let id_col = if mysql {
                "id INT AUTO_INCREMENT PRIMARY KEY"
            } else {
                "id BIGSERIAL PRIMARY KEY"
            };
            Some(format!("CREATE TABLE {ns_q}.{qn} ({id_col});"))
        }
        crate::ui::screens::explorer::CreateKind::View => {
            Some(format!("CREATE VIEW {ns_q}.{qn} AS SELECT 1;"))
        }
        crate::ui::screens::explorer::CreateKind::Type => {
            // MySQL has no standalone CREATE TYPE — report as unsupported.
            if mysql {
                None
            } else {
                Some(format!("CREATE TYPE {ns_q}.{qn} AS ENUM ('value');"))
            }
        }
        crate::ui::screens::explorer::CreateKind::Function => {
            if mysql {
                Some(format!(
                    "CREATE FUNCTION {ns_q}.{qn}() RETURNS INT DETERMINISTIC RETURN 1;"
                ))
            } else {
                Some(format!(
                    "CREATE FUNCTION {ns_q}.{qn}() RETURNS void AS $$ BEGIN END; $$ LANGUAGE plpgsql;"
                ))
            }
        }
    }
}

/// Roadmap M2.10: detect statements that can destroy data before the query
/// console runs them, so the user gets an explicit confirm dialog.
///
/// This is a deliberate heuristic (keyword/prefix scan), NOT a full SQL
/// parser. False positives are preferred over false negatives — an extra
/// confirm on a benign query is a small annoyance; a DROP that executes
/// without confirmation is a data-loss incident.
pub fn is_destructive_statement(query: &str) -> bool {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return false;
    }
    // Use the SAME splitter the executor uses (`split_statements`), so a
    // DROP hidden behind a `-- comment; more` can't slip past the guard
    // while the executor still runs it.
    crate::ui::screens::query::split_statements(query)
        .iter()
        .any(|s| is_destructive_stmt(strip_leading_comments(s)))
}

/// Is a single statement destructive (DROP / TRUNCATE / DELETE without WHERE
/// / ALTER that drops something)?
pub fn is_destructive_stmt(stmt: &str) -> bool {
    let first = stmt
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_uppercase();
    match first.as_str() {
        "DROP" | "TRUNCATE" => true,
        "DELETE" => !has_where_clause(stmt),
        "ALTER" => stmt.to_uppercase().contains(" DROP "),
        _ => false,
    }
}

/// Does a statement contain a top-level `WHERE` keyword — outside string
/// literals, backtick identifiers and `--` / `/* */` comments? Used to allow
/// `DELETE … WHERE` (targeted) through the guard while blocking
/// `DELETE FROM t` (full table). A `WHERE` hidden in `-- WHERE x` or `'WHERE'`
/// must NOT count.
pub fn has_where_clause(stmt: &str) -> bool {
    let mut in_string: Option<char> = None;
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    let mut word = String::new();
    let end_word = |word: &mut String| -> bool {
        let hit = word.eq_ignore_ascii_case("WHERE");
        word.clear();
        hit
    };

    let bytes = stmt.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = stmt[i..].chars().next().unwrap();
        if in_line_comment {
            if c == '\n' {
                in_line_comment = false;
            }
            i += c.len_utf8();
            continue;
        }
        if in_block_comment {
            if c == '*' && bytes.get(i + 1) == Some(&b'/') {
                in_block_comment = false;
                i += 2;
            } else {
                i += c.len_utf8();
            }
            continue;
        }
        if let Some(q) = in_string {
            if c == '\\' {
                i += 1 + c.len_utf8();
                continue;
            }
            if c == q {
                in_string = None;
            }
            i += c.len_utf8();
            continue;
        }
        if c == '\'' || c == '"' || c == '`' {
            in_string = Some(c);
            i += 1;
            continue;
        }
        if c == '-' && bytes.get(i + 1) == Some(&b'-') {
            let after = stmt[i + 2..].chars().next();
            let is_comment = after
                .map(|n| n.is_whitespace() || n.is_control())
                .unwrap_or(true);
            if is_comment {
                in_line_comment = true;
                i += 2;
                continue;
            }
        }
        if c == '/' && bytes.get(i + 1) == Some(&b'*') {
            in_block_comment = true;
            i += 2;
            continue;
        }
        if c.is_whitespace() {
            if end_word(&mut word) {
                return true;
            }
            i += c.len_utf8();
            continue;
        }
        // Punctuation breaks a word so `WHERE(` and `(WHERE` are still found.
        if matches!(c, '(' | ')' | ',' | ';' | '=' | '<' | '>') {
            if end_word(&mut word) {
                return true;
            }
            i += c.len_utf8();
            continue;
        }
        word.push(c);
        i += c.len_utf8();
    }
    word.eq_ignore_ascii_case("WHERE")
}

/// Skip leading whitespace and leading `--` / `/* */` comments so the first
/// real keyword of a statement can be inspected.
pub fn strip_leading_comments(mut s: &str) -> &str {
    loop {
        s = s.trim_start();
        if s.starts_with("--") {
            s = s.lines().nth(1).unwrap_or("");
            continue;
        }
        if s.starts_with("/*") {
            if let Some(end) = s.find("*/") {
                s = &s[end + 2..];
                continue;
            }
        }
        break;
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::{CollectionRef, Namespace, Record, Value};

    fn cref(ns: &str, tbl: &str) -> CollectionRef {
        CollectionRef {
            namespace: Namespace(ns.to_string()),
            name: tbl.to_string(),
        }
    }

    fn row(values: Vec<Value>) -> Record {
        Record { values }
    }

    #[test]
    fn test_quote_ident_by_dialect() {
        // PostgreSQL → double quotes
        assert_eq!(quote_ident("users", "PostgreSQL 15.3"), "\"users\"");
        assert_eq!(quote_ident("order_items", "postgres"), "\"order_items\"");
        // MySQL → backticks
        assert_eq!(quote_ident("users", "MySQL 8.0"), "`users`");
        assert_eq!(quote_ident("order items", "mysql"), "`order items`");
        // SQL Server → brackets
        assert_eq!(quote_ident("users", "Microsoft SQL Server 2019"), "[users]");
        // SQLite → backticks (accepted by SQLite)
        assert_eq!(quote_ident("users", "SQLite 3.45"), "`users`");
        // Unknown driver defaults to PG double-quote
        assert_eq!(quote_ident("users", "MongoDB"), "\"users\"");
    }

    #[test]
    fn test_quote_ident_escapes_inner_quote() {
        // An identifier containing the quote char must double it up.
        assert_eq!(quote_ident("a\"b", "postgres"), "\"a\"\"b\"");
        assert_eq!(quote_ident("a`b", "mysql"), "`a``b`");
        assert_eq!(quote_ident("a]b", "sql server"), "[a]]b]");
    }

    #[test]
    fn test_single_row_suffix_by_dialect() {
        assert_eq!(single_row_suffix("MySQL 8.0"), " LIMIT 1");
        assert_eq!(single_row_suffix("MariaDB 10.11"), " LIMIT 1");
        assert_eq!(single_row_suffix("PostgreSQL 15.3"), "");
        assert_eq!(single_row_suffix("Microsoft SQL Server 2019"), "");
        assert_eq!(single_row_suffix("SQLite 3.45"), "");
    }

    #[test]
    fn test_escape_string_literal_doubles_quotes() {
        assert_eq!(escape_string_literal("plain"), "plain");
        assert_eq!(escape_string_literal("O'Brien"), "O''Brien");
        assert_eq!(escape_string_literal("it's ''quoted''"), "it''s ''''quoted''''");
    }

    #[test]
    fn test_render_buffer_sql_sentinel_and_quotes() {
        assert_eq!(render_buffer_sql(NULL_SENTINEL), "NULL");
        assert_eq!(render_buffer_sql("hello"), "'hello'");
        assert_eq!(render_buffer_sql(""), "''");
        assert_eq!(render_buffer_sql("O'Brien"), "'O''Brien'");
    }

    #[test]
    fn test_build_where_pk_prefer_and_quote() {
        let cols = vec!["id".to_string(), "name".to_string()];
        let rec = row(vec![Value::Int(42), Value::String("ada".to_string())]);
        let pk = vec!["id".to_string()];

        // PostgreSQL: PK column double-quoted, int rendered as quoted literal.
        let sql = build_where_for_row(&cols, &rec, &pk, "PostgreSQL 15.3").unwrap();
        assert_eq!(sql, "\"id\" = '42'");

        // MySQL: backtick.
        let sql = build_where_for_row(&cols, &rec, &pk, "MySQL 8.0").unwrap();
        assert_eq!(sql, "`id` = '42'");
    }

    #[test]
    fn test_build_where_null_pk_uses_is_null() {
        let cols = vec!["id".to_string(), "name".to_string()];
        let rec = row(vec![Value::Null, Value::String("ada".to_string())]);
        let pk = vec!["id".to_string()];
        let sql = build_where_for_row(&cols, &rec, &pk, "postgres").unwrap();
        assert_eq!(sql, "\"id\" IS NULL");
    }

    #[test]
    fn test_build_where_falls_back_to_all_columns() {
        let cols = vec!["a".to_string(), "b".to_string()];
        let rec = row(vec![Value::Int(1), Value::Null]);
        // No PK → every column participates; NULL → IS NULL.
        let sql = build_where_for_row(&cols, &rec, &[], "postgres").unwrap();
        assert_eq!(sql, "\"a\" = '1' AND \"b\" IS NULL");
    }

    #[test]
    fn test_build_where_empty_returns_none() {
        // Zero-column table → no WHERE at all → None (caller bails out).
        assert_eq!(build_where_for_row(&[], &row(vec![]), &[], "postgres"), None);
    }

    #[test]
    fn test_build_insert_skips_none_and_quotes_by_dialect() {
        let c = cref("shop", "orders");
        let fields = vec![
            ("id".to_string(), Some("5".to_string())),
            ("user_id".to_string(), Some(NULL_SENTINEL.to_string())),
            ("note".to_string(), None), // skip → server DEFAULT
        ];

        let pg = build_insert_sql(&c, &fields, "PostgreSQL 15.3").unwrap();
        assert_eq!(
            pg,
            "INSERT INTO \"shop\".\"orders\" (\"id\", \"user_id\") VALUES ('5', NULL);"
        );

        let my = build_insert_sql(&c, &fields, "MySQL 8.0").unwrap();
        assert_eq!(
            my,
            "INSERT INTO `shop`.`orders` (`id`, `user_id`) VALUES ('5', NULL);"
        );
    }

    #[test]
    fn test_build_insert_all_skipped_returns_none() {
        let c = cref("shop", "orders");
        let fields = vec![("id".to_string(), None)];
        assert_eq!(build_insert_sql(&c, &fields, "postgres"), None);
    }

    #[test]
    fn test_generate_alter_sql() {
        let c = cref("shop", "users");
        let sql = generate_alter_sql(
            &c,
            &["email".to_string()],
            &[("bio".to_string(), "TEXT".to_string())],
            &[("name".to_string(), "VARCHAR(64)".to_string())],
            Some("people"),
            "PostgreSQL 15",
        );
        // Rename comes LAST so earlier statements still reference old name.
        let expected = "ALTER TABLE \"shop\".\"users\" DROP COLUMN \"email\";\n\
            ALTER TABLE \"shop\".\"users\" ADD COLUMN \"bio\" TEXT;\n\
            ALTER TABLE \"shop\".\"users\" ALTER COLUMN \"name\" TYPE VARCHAR(64);\n\
            ALTER TABLE \"shop\".\"users\" RENAME TO \"people\";";
        assert_eq!(sql.as_deref(), Some(expected));

        // No operations → None (caller shows a warning instead of empty SQL).
        assert_eq!(
            generate_alter_sql(&c, &[], &[], &[], None, "MySQL 8"),
            None
        );

        // Dialect-aware quoting AND type-change verb (MySQL MODIFY COLUMN).
        let sql = generate_alter_sql(
            &c,
            &[],
            &[("bio".to_string(), "TEXT".to_string())],
            &[("name".to_string(), "VARCHAR(64)".to_string())],
            None,
            "MySQL 8",
        );
        assert_eq!(
            sql.as_deref(),
            Some(
                "ALTER TABLE `shop`.`users` ADD COLUMN `bio` TEXT;\n\
                 ALTER TABLE `shop`.`users` MODIFY COLUMN `name` VARCHAR(64);"
            )
        );
    }

    #[test]
    fn test_build_insert_row_sql_dialect() {
        let columns = vec!["id".to_string(), "name".to_string(), "note".to_string()];
        let row = Record {
            values: vec![
                Value::Int(7),
                Value::String("O'Brien".to_string()),
                Value::Null,
            ],
        };
        let pg = build_insert_row_sql("users", &columns, &row, "PostgreSQL 15.3");
        assert_eq!(
            pg,
            "INSERT INTO \"users\" (\"id\", \"name\", \"note\") VALUES ('7', 'O''Brien', NULL);"
        );
        let my = build_insert_row_sql("users", &columns, &row, "MySQL 8.0");
        assert_eq!(
            my,
            "INSERT INTO `users` (`id`, `name`, `note`) VALUES ('7', 'O''Brien', NULL);"
        );
    }

    #[test]
    fn test_is_destructive_statement() {
        // DROP / TRUNCATE always trip the guard.
        assert!(is_destructive_statement("DROP TABLE users;"));
        assert!(is_destructive_statement("drop database app;"));
        assert!(is_destructive_statement("TRUNCATE TABLE audit_log;"));
        // DELETE without WHERE is destructive.
        assert!(is_destructive_statement("DELETE FROM users;"));
        assert!(is_destructive_statement("DELETE FROM users"));
        // DELETE with WHERE is allowed through.
        assert!(!is_destructive_statement("DELETE FROM users WHERE id = 5;"));
        // ALTER that DROPs a column is destructive.
        assert!(is_destructive_statement("ALTER TABLE users DROP COLUMN email;"));
        // ALTER that ADDs is safe.
        assert!(!is_destructive_statement("ALTER TABLE users ADD COLUMN bio TEXT;"));
        // A benign SELECT is safe even if a later chunk is destructive.
        assert!(is_destructive_statement("SELECT * FROM users; DROP TABLE users;"));
        // Case-insensitive.
        assert!(is_destructive_statement("  drop  table  users  "));
        // Guard bypass: DROP hidden after a comment containing ';' (the old
        // naive split() let this through; split_statements + strip does not).
        assert!(is_destructive_statement("SELECT 1; -- note; more\nDROP TABLE users"));
        assert!(is_destructive_statement("-- note\nDROP TABLE users"));
        assert!(is_destructive_statement("/* hi */ DELETE FROM users"));
        // Safe DELETE with WHERE stays safe even behind a comment.
        assert!(!is_destructive_statement("-- safe\nDELETE FROM users WHERE id = 1"));
        // Empty / whitespace → safe.
        assert!(!is_destructive_statement(""));
        assert!(!is_destructive_statement("   "));
    }
}

//! Multi-format export engine for tabular data.
//! Supports exporting datasets to CSV (RFC 4180), formatted JSON, and SQL INSERT scripts.

use std::path::{Path, PathBuf};
use anyhow::{Context, Result};
use serde_json::{Map, Value as JsonValue};

use crate::clipboard::value_to_json;
use crate::driver::{Record, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportFormat {
    Csv,
    Json,
    SqlInsert,
    SqlDump,
    Xlsx,
}

impl ExportFormat {
    pub const ALL: [ExportFormat; 5] = [
        ExportFormat::Csv,
        ExportFormat::Json,
        ExportFormat::SqlInsert,
        ExportFormat::SqlDump,
        ExportFormat::Xlsx,
    ];

    pub fn name(&self) -> &'static str {
        match self {
            ExportFormat::Csv => "CSV",
            ExportFormat::Json => "JSON",
            ExportFormat::SqlInsert => "SQL INSERT",
            ExportFormat::SqlDump => "SQL dump (DDL + INSERTs)",
            ExportFormat::Xlsx => "Excel (xlsx)",
        }
    }

    pub fn extension(&self) -> &'static str {
        match self {
            ExportFormat::Csv => "csv",
            ExportFormat::Json => "json",
            ExportFormat::SqlInsert => "sql",
            ExportFormat::SqlDump => "sql",
            ExportFormat::Xlsx => "xlsx",
        }
    }
}

pub struct Exporter;

impl Exporter {
    /// Formats dataset as RFC 4180 CSV
    pub fn format_csv(columns: &[String], records: &[Record]) -> String {
        let mut out = String::new();

        // Header line
        let header = columns
            .iter()
            .map(|c| escape_csv_field(c))
            .collect::<Vec<_>>()
            .join(",");
        out.push_str(&header);
        out.push('\n');

        // Data lines
        for rec in records {
            let row_str = rec
                .values
                .iter()
                .map(|val| match val {
                    Value::Null => String::new(),
                    _ => escape_csv_field(&val.display_str()),
                })
                .collect::<Vec<_>>()
                .join(",");
            out.push_str(&row_str);
            out.push('\n');
        }

        out
    }

    /// Formats dataset as an array of JSON objects
    pub fn format_json(columns: &[String], records: &[Record]) -> Result<String> {
        let mut rows = Vec::with_capacity(records.len());

        for rec in records {
            let mut map = Map::new();
            for (i, col) in columns.iter().enumerate() {
                let val = rec.values.get(i).unwrap_or(&Value::Null);
                map.insert(col.clone(), value_to_json(val));
            }
            rows.push(JsonValue::Object(map));
        }

        serde_json::to_string_pretty(&JsonValue::Array(rows))
            .context("failed to serialize export dataset to JSON")
    }

    /// Formats dataset as standard SQL `INSERT INTO ...` statements
    pub fn format_sql_insert(table_name: &str, columns: &[String], records: &[Record]) -> String {
        if records.is_empty() || columns.is_empty() {
            return format!("-- No records to export for table `{table_name}`\n");
        }

        let mut out = String::new();
        let cols_escaped = columns
            .iter()
            .map(|c| format!("`{}`", c.replace('`', "``")))
            .collect::<Vec<_>>()
            .join(", ");

        out.push_str(&format!("-- Exported by dbx\n-- Table: `{table_name}`\n\n"));

        for rec in records {
            let vals = rec
                .values
                .iter()
                .map(|v| escape_sql_value(v))
                .collect::<Vec<_>>()
                .join(", ");

            out.push_str(&format!(
                "INSERT INTO `{}` ({}) VALUES ({});\n",
                table_name.replace('`', "``"),
                cols_escaped,
                vals
            ));
        }

        out
    }

    /// Formats dataset as a restorable dump: the table's DDL (when the driver
    /// could supply it) followed by one INSERT per row. `ddl = None` happens
    /// for console-result exports, which have no originating table.
    pub fn format_sql_dump(
        table_name: &str,
        ddl: Option<&str>,
        columns: &[String],
        records: &[Record],
    ) -> String {
        let mut out = format!("-- Exported by dbx\n-- Table: `{table_name}`\n\n");
        match ddl {
            Some(ddl) if !ddl.trim().is_empty() => {
                out.push_str("-- Structure\n");
                out.push_str(&format!(
                    "DROP TABLE IF EXISTS `{}`;\n",
                    table_name.replace('`', "``")
                ));
                out.push_str(ddl.trim_end());
                if !ddl.trim_end().ends_with(';') {
                    out.push(';');
                }
                out.push_str("\n\n-- Data\n");
            }
            _ => {
                out.push_str("-- Structure unavailable (no source table) — INSERTs only\n\n");
            }
        }
        if records.is_empty() || columns.is_empty() {
            out.push_str("-- No records to export\n");
            return out;
        }

        let cols_escaped = columns
            .iter()
            .map(|c| format!("`{}`", c.replace('`', "``")))
            .collect::<Vec<_>>()
            .join(", ");

        for rec in records {
            let vals = rec
                .values
                .iter()
                .map(|v| escape_sql_value(v))
                .collect::<Vec<_>>()
                .join(", ");

            out.push_str(&format!(
                "INSERT INTO `{}` ({}) VALUES ({});\n",
                table_name.replace('`', "``"),
                cols_escaped,
                vals
            ));
        }

        out
    }

    /// Formats the dataset as an Excel workbook (one sheet named `Data`).
    /// Numbers and booleans become real Excel cells; everything else is text.
    pub fn format_xlsx(columns: &[String], records: &[Record]) -> Result<Vec<u8>> {
        let mut workbook = rust_xlsxwriter::Workbook::new();
        let sheet = workbook.add_worksheet().set_name("Data")?;

        for (i, col) in columns.iter().enumerate() {
            sheet.write_string(0, i as u16, col)?;
        }
        for (r, rec) in records.iter().enumerate() {
            let row = (r + 1) as u32;
            for (c, val) in rec.values.iter().enumerate() {
                let col = c as u16;
                match val {
                    Value::Null => {}
                    Value::Bool(b) => {
                        sheet.write_boolean(row, col, *b)?;
                    }
                    Value::Int(n) => {
                        sheet.write_number(row, col, *n as f64)?;
                    }
                    Value::UInt(u) => {
                        sheet.write_number(row, col, *u as f64)?;
                    }
                    Value::Float(f) => {
                        sheet.write_number(row, col, *f)?;
                    }
                    // Decimal/DateTime/Json stay text: a float round-trip or a
                    // locale-dependent date parse would corrupt the value.
                    other => {
                        sheet.write_string(row, col, &other.display_str())?;
                    }
                }
            }
        }

        workbook
            .save_to_buffer()
            .context("failed to build the xlsx workbook")
    }

    /// Expands `~` to the user's home directory and writes content to disk
    pub fn save_to_file(path_str: &str, content: &str) -> Result<PathBuf> {
        let resolved_path = resolve_path(path_str)?;

        if let Some(parent) = resolved_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create parent directories for {:?}", parent))?;
        }

        std::fs::write(&resolved_path, content)
            .with_context(|| format!("failed to write exported data to {:?}", resolved_path))?;

        Ok(resolved_path)
    }

    /// Same as [`save_to_file`] but for binary payloads (xlsx).
    pub fn save_bytes_to_file(path_str: &str, content: &[u8]) -> Result<PathBuf> {
        let resolved_path = resolve_path(path_str)?;

        if let Some(parent) = resolved_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create parent directories for {:?}", parent))?;
        }

        std::fs::write(&resolved_path, content)
            .with_context(|| format!("failed to write exported data to {:?}", resolved_path))?;

        Ok(resolved_path)
    }
}

/// Escapes a field for CSV format per RFC 4180
fn escape_csv_field(field: &str) -> String {
    if field.contains(',') || field.contains('"') || field.contains('\n') || field.contains('\r') {
        let escaped = field.replace('"', "\"\"");
        format!("\"{escaped}\"")
    } else {
        field.to_string()
    }
}

/// Escapes a `Value` for SQL literals
fn escape_sql_value(val: &Value) -> String {
    match val {
        Value::Null => "NULL".to_string(),
        Value::Bool(b) => if *b { "TRUE" } else { "FALSE" }.to_string(),
        Value::Int(n) => n.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Decimal(s) => s.clone(),
        Value::String(s) => {
            let escaped = s.replace('\\', "\\\\").replace('\'', "''");
            format!("'{escaped}'")
        }
        Value::Bytes(b) => {
            // Hex literal X'...'
            let hex_str = b.iter().map(|byte| format!("{:02X}", byte)).collect::<String>();
            format!("X'{hex_str}'")
        }
        Value::Json(j) => {
            let s = j.to_string().replace('\\', "\\\\").replace('\'', "''");
            format!("'{s}'")
        }
        Value::DateTime(dt) => {
            let escaped = dt.replace('\\', "\\\\").replace('\'', "''");
            format!("'{escaped}'")
        }
    }
}

/// Resolves user home directory `~` or relative paths into an absolute PathBuf
pub fn resolve_path(path_str: &str) -> Result<PathBuf> {
    let path = Path::new(path_str);
    if path.starts_with("~") {
        let home = dirs::home_dir().context("could not determine user home directory")?;
        let sub = path.strip_prefix("~").unwrap_or(path);
        Ok(home.join(sub))
    } else {
        Ok(std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf()))
    }
}

/// Parse a CSV string into rows of fields, honouring double-quoted fields
/// (quotes, commas and doubled `""` inside quotes), multi-line quoted
/// fields (RFC 4180), UTF-8 BOM and CRLF. Blank lines are skipped.
pub fn parse_csv(content: &str) -> Vec<Vec<String>> {
    let content = content.trim_start_matches('\u{feff}');
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;

    let mut chars = content.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    field.push('"');
                    chars.next();
                } else {
                    in_quotes = false;
                }
            } else {
                field.push(c);
            }
            continue;
        }
        match c {
            '"' => in_quotes = true,
            ',' => row.push(std::mem::take(&mut field)),
            '\n' => {
                row.push(std::mem::take(&mut field));
                if row.iter().any(|f| !f.is_empty()) {
                    rows.push(std::mem::take(&mut row));
                }
            }
            '\r' => {} // CRLF: drop the CR
            _ => field.push(c),
        }
    }
    // Last line (no trailing newline).
    if !field.is_empty() || !row.is_empty() {
        row.push(std::mem::take(&mut field));
        if row.iter().any(|f| !f.is_empty()) {
            rows.push(row);
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_csv_basic() {
        let rows = parse_csv("a,b,c\n1,2,3\n");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], vec!["a".to_string(), "b".to_string(), "c".to_string()]);
        assert_eq!(rows[1], vec!["1".to_string(), "2".to_string(), "3".to_string()]);
    }

    #[test]
    fn test_parse_csv_quoted_fields() {
        // Commas and quotes inside double-quoted fields survive.
        let rows = parse_csv("\"a,b\",\"say \"\"hi\"\"\"\nx,y\n");
        assert_eq!(rows[0], vec!["a,b".to_string(), "say \"hi\"".to_string()]);
        // Blank lines skipped.
        assert_eq!(parse_csv("a\n\nb\n").len(), 2);
    }

    #[test]
    fn test_parse_csv_multiline_bom_crlf() {
        // UTF-8 BOM stripped from the first header cell.
        let rows = parse_csv("\u{feff}id,name\n1,x\n");
        assert_eq!(rows[0][0], "id");

        // RFC 4180 multi-line quoted field survives as one field.
        let rows = parse_csv("a,b\nx,\"line1\nline2\"\n");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1][1], "line1\nline2");

        // CRLF handled.
        let rows = parse_csv("a,b\r\n1,2\r\n");
        assert_eq!(rows[1], vec!["1".to_string(), "2".to_string()]);
    }

    #[test]
    fn test_csv_export_escaping() {
        let cols = vec!["id".to_string(), "notes".to_string(), "city".to_string()];
        let recs = vec![
            Record {
                values: vec![
                    Value::Int(1),
                    Value::String("Line 1\nLine 2 with \"quotes\"".to_string()),
                    Value::String("New York, NY".to_string()),
                ],
            },
            Record {
                values: vec![
                    Value::Int(2),
                    Value::Null,
                    Value::String("Tokyo".to_string()),
                ],
            },
        ];

        let csv = Exporter::format_csv(&cols, &recs);
        assert!(csv.starts_with("id,notes,city\n"));
        assert!(csv.contains("\"Line 1\nLine 2 with \"\"quotes\"\"\""));
        assert!(csv.contains("\"New York, NY\""));
        assert!(csv.contains("2,,Tokyo"));
    }

    #[test]
    fn test_sql_insert_export() {
        let cols = vec!["id".to_string(), "username".to_string(), "is_admin".to_string()];
        let recs = vec![
            Record {
                values: vec![
                    Value::Int(100),
                    Value::String("O'Connor".to_string()),
                    Value::Bool(true),
                ],
            },
        ];

        let sql = Exporter::format_sql_insert("users", &cols, &recs);
        assert!(sql.contains("INSERT INTO `users` (`id`, `username`, `is_admin`) VALUES (100, 'O''Connor', TRUE);"));
    }

    #[test]
    fn test_sql_dump_includes_ddl_and_inserts() {
        let cols = vec!["id".to_string()];
        let recs = vec![Record { values: vec![Value::Int(1)] }];
        let dump = Exporter::format_sql_dump(
            "users",
            Some("CREATE TABLE `users` (`id` int)"),
            &cols,
            &recs,
        );
        assert!(dump.contains("DROP TABLE IF EXISTS `users`;"));
        // A DDL without its terminating semicolon gets one.
        assert!(dump.contains("CREATE TABLE `users` (`id` int);\n"));
        assert!(dump.contains("INSERT INTO `users` (`id`) VALUES (1);"));

        // A DDL already ending in `;` is left alone.
        let dump = Exporter::format_sql_dump("users", Some("CREATE TABLE t (id int);"), &cols, &recs);
        assert!(dump.contains("CREATE TABLE t (id int);\n"));
        assert!(!dump.contains(";;"));
    }

    #[test]
    fn test_sql_dump_without_ddl_falls_back_to_inserts() {
        let cols = vec!["id".to_string()];
        let recs = vec![Record { values: vec![Value::Int(1)] }];
        let dump = Exporter::format_sql_dump("query_result", None, &cols, &recs);
        assert!(dump.contains("Structure unavailable"));
        assert!(dump.contains("INSERT INTO `query_result` (`id`) VALUES (1);"));
        assert!(!dump.contains("DROP TABLE"));
    }

    #[test]
    fn test_xlsx_export_produces_a_workbook() {
        let cols = vec!["id".to_string(), "name".to_string(), "active".to_string()];
        let recs = vec![Record {
            values: vec![
                Value::Int(7),
                Value::String("ada".to_string()),
                Value::Bool(true),
            ],
        }];
        // An xlsx is a zip container — the magic bytes prove the writer ran.
        let bytes = Exporter::format_xlsx(&cols, &recs).unwrap();
        assert!(bytes.len() > 4);
        assert_eq!(&bytes[..2], b"PK");
    }
}

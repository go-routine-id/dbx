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
}

impl ExportFormat {
    pub const ALL: [ExportFormat; 3] = [ExportFormat::Csv, ExportFormat::Json, ExportFormat::SqlInsert];

    pub fn name(&self) -> &'static str {
        match self {
            ExportFormat::Csv => "CSV",
            ExportFormat::Json => "JSON",
            ExportFormat::SqlInsert => "SQL INSERT",
        }
    }

    pub fn extension(&self) -> &'static str {
        match self {
            ExportFormat::Csv => "csv",
            ExportFormat::Json => "json",
            ExportFormat::SqlInsert => "sql",
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

#[cfg(test)]
mod tests {
    use super::*;

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
}

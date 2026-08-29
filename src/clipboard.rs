//! System Clipboard integration via `arboard`.
//! Safe non-panicking wrappers for copying cell values and full rows (as TSV / JSON).

use arboard::Clipboard;
use serde_json::{Map, Value as JsonValue};

use crate::driver::{Record, Value};

pub struct ClipboardManager;

impl ClipboardManager {
    /// Copies arbitrary plain text to the system clipboard.
    pub fn set_text(text: &str) -> Result<(), String> {
        match Clipboard::new() {
            Ok(mut cb) => cb.set_text(text.to_string()).map_err(|e| format!("Clipboard error: {e}")),
            Err(e) => Err(format!("Failed to access clipboard: {e}")),
        }
    }

    /// Copies a single cell's display value to the clipboard.
    pub fn copy_cell(val: &Value) -> Result<(), String> {
        Self::set_text(&val.display_str())
    }

    /// Formats and copies a row as a tab-separated string (TSV).
    pub fn copy_row_tsv(record: &Record) -> Result<(), String> {
        let text = format_row_tsv(record);
        Self::set_text(&text)
    }

    /// Formats and copies a row as a formatted JSON object string.
    pub fn copy_row_json(columns: &[String], record: &Record) -> Result<(), String> {
        let text = format_row_json(columns, record)?;
        Self::set_text(&text)
    }
}

/// Formats a record as TSV
pub fn format_row_tsv(record: &Record) -> String {
    record
        .values
        .iter()
        .map(|v| v.display_str())
        .collect::<Vec<_>>()
        .join("\t")
}

/// Formats a row as a JSON object string
pub fn format_row_json(columns: &[String], record: &Record) -> Result<String, String> {
    let mut map = Map::new();
    for (i, col) in columns.iter().enumerate() {
        let val = record.values.get(i).unwrap_or(&Value::Null);
        map.insert(col.clone(), value_to_json(val));
    }
    serde_json::to_string_pretty(&JsonValue::Object(map))
        .map_err(|e| format!("JSON serialization error: {e}"))
}

/// Converts a domain `Value` into `serde_json::Value`
pub fn value_to_json(val: &Value) -> JsonValue {
    match val {
        Value::Null => JsonValue::Null,
        Value::Bool(b) => JsonValue::Bool(*b),
        Value::Int(n) => JsonValue::Number((*n).into()),
        Value::UInt(u) => JsonValue::Number((*u).into()),
        Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(JsonValue::Number)
            .unwrap_or_else(|| JsonValue::String(f.to_string())),
        Value::Decimal(s) => JsonValue::String(s.clone()),
        Value::String(s) => JsonValue::String(s.clone()),
        Value::Bytes(b) => JsonValue::String(format!("<bytes: {} len>", b.len())),
        Value::Json(j) => j.clone(),
        Value::DateTime(dt) => JsonValue::String(dt.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_row_tsv() {
        let rec = Record {
            values: vec![
                Value::Int(42),
                Value::String("hello world".to_string()),
                Value::Null,
                Value::Bool(true),
            ],
        };
        let tsv = format_row_tsv(&rec);
        assert_eq!(tsv, "42\thello world\tNULL\ttrue");
    }

    #[test]
    fn test_format_row_json() {
        let cols = vec!["id".to_string(), "name".to_string(), "active".to_string()];
        let rec = Record {
            values: vec![
                Value::Int(10),
                Value::String("Alice".to_string()),
                Value::Bool(false),
            ],
        };
        let json_str = format_row_json(&cols, &rec).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed["id"], 10);
        assert_eq!(parsed["name"], "Alice");
        assert_eq!(parsed["active"], false);
    }
}

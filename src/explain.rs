//! Query-plan parsing for the `EXPLAIN` viewer.
//!
//! Every engine reports plans differently, so this module turns each dialect's
//! output into one shape — a flat list of [`PlanNode`]s carrying their tree
//! depth — that the UI can render identically:
//!
//! * **PostgreSQL** — one text column; `->` markers and indentation encode the
//!   tree, and each node carries `cost=..` / `rows=..`.
//! * **SQLite** — `EXPLAIN QUERY PLAN` returns `id`/`parent`/`detail`, so the
//!   tree is explicit and depth is derived by walking parents.
//! * **MySQL** — a flat table (one row per accessed table), so every node sits
//!   at depth 0 and the columns are folded into the label.

use crate::driver::QueryResult;

/// One node of a query plan, flattened with its depth.
#[derive(Clone, Debug, PartialEq)]
pub struct PlanNode {
    pub depth: usize,
    pub label: String,
    /// Estimated total cost, when the engine reports one.
    pub cost: Option<f64>,
    /// Estimated row count, when the engine reports one.
    pub rows: Option<f64>,
}

/// Wrap `query` in the dialect's EXPLAIN form.
pub fn explain_sql(driver_name: &str, query: &str) -> String {
    let q = query.trim().trim_end_matches(';');
    let lower = driver_name.to_lowercase();
    if lower.contains("sqlite") {
        // The plain `EXPLAIN` dumps VDBE opcodes, which is not what anyone
        // wants to read; the query plan is the useful view.
        format!("EXPLAIN QUERY PLAN {q}")
    } else if lower.contains("clickhouse") {
        // clickhouse: EXPLAIN PLAN yields the indented-text plan the default
        // (Postgres-style) parser can flatten; plain EXPLAIN also works but
        // being explicit guards against server default changes.
        format!("EXPLAIN PLAN {q}")
    } else {
        format!("EXPLAIN {q}")
    }
}

/// Parse an EXPLAIN result into plan nodes, picking the dialect by driver name.
pub fn parse_plan(driver_name: &str, res: &QueryResult) -> Vec<PlanNode> {
    let lower = driver_name.to_lowercase();
    if lower.contains("sqlite") {
        parse_sqlite(res)
    } else if lower.contains("mysql") || lower.contains("maria") {
        parse_tabular(res)
    } else {
        parse_postgres(res)
    }
}

/// Pull `cost=a..b` and `rows=n` out of a PostgreSQL plan line.
fn parse_costs(text: &str) -> (Option<f64>, Option<f64>) {
    let num_after = |key: &str| -> Option<f64> {
        let start = text.find(key)? + key.len();
        let rest = &text[start..];
        // PG writes `cost=0.00..18.50`; the total is the second number.
        let end = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        let head = &rest[..end];
        let last = head.rsplit("..").next().unwrap_or(head);
        last.trim_end_matches('.').parse().ok()
    };
    (num_after("cost="), num_after("rows="))
}

/// PostgreSQL: a single text column where indentation and `->` build the tree.
fn parse_postgres(res: &QueryResult) -> Vec<PlanNode> {
    let mut nodes = Vec::new();
    for record in &res.records {
        let Some(cell) = record.values.first() else {
            continue;
        };
        let line = cell.display_str();
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            continue;
        }
        let indent = line.len() - trimmed.len();

        if let Some(rest) = trimmed.strip_prefix("->") {
            // PG indents each nesting level by 6 columns for `->` rows.
            let label = rest.trim().to_string();
            let (cost, rows) = parse_costs(&label);
            nodes.push(PlanNode {
                depth: indent / 6 + 1,
                label,
                cost,
                rows,
            });
        } else if indent == 0 {
            // The root node (or a trailing "Planning Time:" style footer).
            let (cost, rows) = parse_costs(trimmed);
            nodes.push(PlanNode {
                depth: 0,
                label: trimmed.to_string(),
                cost,
                rows,
            });
        } else {
            // Detail lines ("Hash Cond: ...", "Filter: ...") belong to the
            // node above them; keep them one level deeper so they read as
            // attributes rather than plan steps.
            let depth = nodes.last().map(|n: &PlanNode| n.depth + 1).unwrap_or(1);
            nodes.push(PlanNode {
                depth,
                label: trimmed.to_string(),
                cost: None,
                rows: None,
            });
        }
    }
    nodes
}

/// SQLite `EXPLAIN QUERY PLAN`: explicit `id` / `parent` columns.
fn parse_sqlite(res: &QueryResult) -> Vec<PlanNode> {
    let col = |name: &str| res.columns.iter().position(|c| c.eq_ignore_ascii_case(name));
    let (Some(id_i), Some(parent_i), Some(detail_i)) =
        (col("id"), col("parent"), col("detail"))
    else {
        // Column names differ across versions — fall back to a flat list of
        // the last column, which is always the human-readable detail.
        return parse_tabular(res);
    };

    // depth(node) = depth(parent) + 1; parent 0 means "top level".
    let mut depth_of: std::collections::HashMap<i64, usize> = std::collections::HashMap::new();
    let mut nodes = Vec::new();
    for record in &res.records {
        let num = |i: usize| -> Option<i64> { record.values.get(i)?.display_str().parse().ok() };
        let id = num(id_i).unwrap_or(0);
        let parent = num(parent_i).unwrap_or(0);
        let depth = depth_of.get(&parent).map(|d| d + 1).unwrap_or(0);
        depth_of.insert(id, depth);
        nodes.push(PlanNode {
            depth,
            label: record
                .values
                .get(detail_i)
                .map(|v| v.display_str())
                .unwrap_or_default(),
            cost: None,
            rows: None,
        });
    }
    nodes
}

/// MySQL and any unknown tabular plan: one flat node per row, with the
/// columns folded into `col=value` pairs so nothing is lost.
fn parse_tabular(res: &QueryResult) -> Vec<PlanNode> {
    res.records
        .iter()
        .map(|record| {
            let label = res
                .columns
                .iter()
                .zip(record.values.iter())
                .filter(|(_, v)| !matches!(v, crate::driver::Value::Null))
                .map(|(c, v)| format!("{c}={}", v.display_str()))
                .collect::<Vec<_>>()
                .join("  ");
            let rows = res
                .columns
                .iter()
                .position(|c| c.eq_ignore_ascii_case("rows"))
                .and_then(|i| record.values.get(i))
                .and_then(|v| v.display_str().parse().ok());
            PlanNode {
                depth: 0,
                label,
                cost: None,
                rows,
            }
        })
        .collect()
}

/// Index of the costliest node, used to highlight the bottleneck. Falls back
/// to the largest row estimate when the engine reports no cost.
pub fn hotspot(nodes: &[PlanNode]) -> Option<usize> {
    let by = |f: fn(&PlanNode) -> Option<f64>| {
        nodes
            .iter()
            .enumerate()
            .filter_map(|(i, n)| f(n).map(|v| (i, v)))
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(i, _)| i)
    };
    by(|n| n.cost).or_else(|| by(|n| n.rows))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::{Record, Value};

    fn result(columns: &[&str], rows: Vec<Vec<Value>>) -> QueryResult {
        QueryResult {
            columns: columns.iter().map(|c| c.to_string()).collect(),
            records: rows.into_iter().map(|values| Record { values }).collect(),
            rows_affected: 0,
            execution_time: std::time::Duration::ZERO,
        }
    }

    fn text_result(lines: &[&str]) -> QueryResult {
        result(
            &["QUERY PLAN"],
            lines
                .iter()
                .map(|l| vec![Value::String((*l).to_string())])
                .collect(),
        )
    }

    #[test]
    fn test_explain_sql_per_dialect() {
        assert_eq!(
            explain_sql("PostgreSQL 15.3", "SELECT 1;"),
            "EXPLAIN SELECT 1"
        );
        assert_eq!(explain_sql("MySQL 8.0", "SELECT 1"), "EXPLAIN SELECT 1");
        // SQLite's bare EXPLAIN dumps opcodes, so the plan form is used.
        assert_eq!(
            explain_sql("SQLite", "SELECT 1"),
            "EXPLAIN QUERY PLAN SELECT 1"
        );
    }

    #[test]
    fn test_parse_postgres_builds_tree_with_costs() {
        let res = text_result(&[
            "Hash Join  (cost=1.09..2.19 rows=5 width=64)",
            "  Hash Cond: (a.id = b.id)",
            "  ->  Seq Scan on a  (cost=0.00..1.05 rows=5 width=32)",
            "  ->  Hash  (cost=1.04..1.04 rows=4 width=32)",
            "        ->  Seq Scan on b  (cost=0.00..1.04 rows=4 width=32)",
        ]);
        let plan = parse_plan("PostgreSQL 15.3", &res);

        assert_eq!(plan[0].depth, 0);
        assert!(plan[0].label.starts_with("Hash Join"));
        // Cost is the TOTAL (second number), not the startup cost.
        assert_eq!(plan[0].cost, Some(2.19));
        assert_eq!(plan[0].rows, Some(5.0));

        // A detail line hangs under the node above it.
        assert_eq!(plan[1].label, "Hash Cond: (a.id = b.id)");
        assert_eq!(plan[1].depth, 1);
        assert_eq!(plan[1].cost, None);

        // `->` children sit one level under the root; the deeper one nests further.
        assert_eq!(plan[2].depth, 1);
        assert!(plan[2].label.starts_with("Seq Scan on a"));
        assert!(plan[4].depth > plan[3].depth, "nested scan must be deeper");
    }

    #[test]
    fn test_parse_sqlite_uses_parent_links_for_depth() {
        let res = result(
            &["id", "parent", "notused", "detail"],
            vec![
                vec![
                    Value::Int(2),
                    Value::Int(0),
                    Value::Int(0),
                    Value::String("SCAN users".into()),
                ],
                vec![
                    Value::Int(4),
                    Value::Int(2),
                    Value::Int(0),
                    Value::String("USE TEMP B-TREE".into()),
                ],
            ],
        );
        let plan = parse_plan("SQLite", &res);
        assert_eq!(plan[0].depth, 0);
        assert_eq!(plan[0].label, "SCAN users");
        // Child of node 2 -> one level deeper.
        assert_eq!(plan[1].depth, 1);
    }

    #[test]
    fn test_parse_mysql_is_flat_and_keeps_columns() {
        let res = result(
            &["table", "type", "rows", "Extra"],
            vec![vec![
                Value::String("users".into()),
                Value::String("ALL".into()),
                Value::Int(850),
                Value::Null,
            ]],
        );
        let plan = parse_plan("MySQL 8.0", &res);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].depth, 0);
        assert!(plan[0].label.contains("table=users"));
        assert!(plan[0].label.contains("type=ALL"));
        // NULL columns are dropped rather than printed as noise.
        assert!(!plan[0].label.contains("Extra"));
        assert_eq!(plan[0].rows, Some(850.0));
    }

    #[test]
    fn test_hotspot_prefers_cost_then_rows() {
        let costly = vec![
            PlanNode { depth: 0, label: "a".into(), cost: Some(1.0), rows: Some(900.0) },
            PlanNode { depth: 1, label: "b".into(), cost: Some(99.0), rows: Some(1.0) },
        ];
        assert_eq!(hotspot(&costly), Some(1));

        // With no costs reported (MySQL / SQLite), the row estimate decides.
        let rows_only = vec![
            PlanNode { depth: 0, label: "a".into(), cost: None, rows: Some(10.0) },
            PlanNode { depth: 0, label: "b".into(), cost: None, rows: Some(500.0) },
        ];
        assert_eq!(hotspot(&rows_only), Some(1));

        assert_eq!(hotspot(&[]), None);
    }
}

//! Console text → statements.
//!
//! Splitting console text is a domain concern, not a UI one: the drivers and
//! the destructive guard need it as much as the editor does. Keeping it here
//! means `sql.rs` and `driver/` never reach into `ui::`.
//!
//! Each driver declares a [`ConsoleDialect`]; only the SQL one uses `;`.

use crate::driver::ConsoleDialect;

/// If `s[i..]` begins a PostgreSQL dollar-quoted string (`$$…$$` or
/// `$tag$…$tag$`), return the byte index just past its closing delimiter.
/// Otherwise return `None` (the `$` is a normal character, e.g. a parameter).
pub fn dollar_quote_end(s: &str, i: usize) -> Option<usize> {
    let bytes = s.as_bytes();
    if bytes.get(i) != Some(&b'$') {
        return None;
    }
    // Parse the opening delimiter: `$$` (empty tag) or `$ident$`.
    let mut j = i + 1;
    if bytes.get(j) == Some(&b'$') {
        j += 1;
    } else {
        let first = *bytes.get(j)?;
        if !(first.is_ascii_alphabetic() || first == b'_') {
            return None;
        }
        j += 1;
        while let Some(&b) = bytes.get(j) {
            if b.is_ascii_alphanumeric() || b == b'_' {
                j += 1;
            } else {
                break;
            }
        }
        if bytes.get(j) != Some(&b'$') {
            return None;
        }
        j += 1;
    }
    let tag = &s[i..j];
    let close = s[j..].find(tag)?;
    Some(j + close + tag.len())
}

/// Byte ranges of the `;`-separated statements in a query, ignoring `;`
/// inside string literals (incl. backslash-escaped quotes), backtick
/// identifiers, `--` line comments (only when followed by whitespace, per
/// the SQL standard / MySQL), `/* */` block comments, and PostgreSQL
/// `$tag$` dollar-quoted bodies.
///
/// Ranges rather than strings so a caller can ask which statement the caret
/// is in; [`split_statements`] trims them into the text to execute.
fn sql_spans(sql: &str) -> Vec<std::ops::Range<usize>> {
    let mut stmts: Vec<std::ops::Range<usize>> = Vec::new();
    let mut start = 0usize;
    let mut in_string: Option<char> = None;
    let mut in_line_comment = false;
    let mut in_block_comment = false;

    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = sql[i..].chars().next().unwrap();
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
            // Backslash escape keeps the next char from closing the string
            // (MySQL `\'`). Consume it so it isn't re-scanned.
            if c == '\\' {
                match sql[i + 1..].chars().next() {
                    Some(nc) => i += 1 + nc.len_utf8(),
                    None => i += 1,
                }
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
        // PostgreSQL dollar-quote: a `;` inside `$$…$$` / `$tag$…$tag$` is
        // part of the body and must not split the statement.
        if c == '$' && let Some(end) = dollar_quote_end(sql, i) {
            i = end;
            continue;
        }
        // `--` starts a comment only when followed by whitespace/control
        // (SQL standard; MySQL allows `a--b` as a - (-b)).
        if c == '-' && bytes.get(i + 1) == Some(&b'-') {
            let after = sql[i + 2..].chars().next();
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
        if c == ';' {
            stmts.push(start..i);
            start = i + 1;
            i += 1;
            continue;
        }
        i += c.len_utf8();
    }
    stmts.push(start..sql.len());
    stmts
}

/// Byte ranges of the statements in console text, in the dialect the target
/// server reads. Ranges may include surrounding whitespace and comments.
pub fn statement_spans(dialect: ConsoleDialect, text: &str) -> Vec<std::ops::Range<usize>> {
    match dialect {
        ConsoleDialect::Sql => sql_spans(text),
        ConsoleDialect::RedisCommand => command_line_spans(text),
        ConsoleDialect::MongoJson => json_object_spans(text),
    }
}

/// Split console text into statements the way the target server reads it.
///
/// Only SQL uses `;`. Redis takes one command per line, and MongoDB one JSON
/// object per command — feeding either through the SQL splitter merged a
/// whole script into a single unparseable statement.
pub fn split_statements_for(dialect: ConsoleDialect, text: &str) -> Vec<String> {
    statement_spans(dialect, text)
        .into_iter()
        .filter_map(|r| {
            let s = text[r].trim();
            (!s.is_empty() && !is_dropped(dialect, s)).then(|| s.to_string())
        })
        .collect()
}

/// `;`-separated SQL statements — the dialect every SQL driver uses.
pub fn split_statements(sql: &str) -> Vec<String> {
    split_statements_for(ConsoleDialect::Sql, sql)
}

/// Text that is a comment for its dialect rather than a statement.
fn is_dropped(dialect: ConsoleDialect, stmt: &str) -> bool {
    dialect == ConsoleDialect::RedisCommand && stmt.starts_with('#')
}

/// The statement the caret sits in, with its range.
///
/// A caret in the whitespace *after* a statement belongs to that statement —
/// parking on the blank line under a query and pressing run is a request to
/// run the query above, not nothing. A caret before the first statement takes
/// the first one.
pub fn statement_at(
    dialect: ConsoleDialect,
    text: &str,
    offset: usize,
) -> Option<(std::ops::Range<usize>, String)> {
    // Trimmed ranges, not raw spans: a span starts right after the previous
    // `;`, so the blank line UNDER a statement belongs to the next one. Using
    // raw spans made a caret parked below `SELECT …;` run the statement after
    // it — the opposite of what this function promises.
    let spans: Vec<std::ops::Range<usize>> = statement_spans(dialect, text)
        .into_iter()
        .filter_map(|r| trimmed_range(text, r))
        .filter(|r| {
            let s = &text[r.clone()];
            !is_dropped(dialect, s) && !(dialect == ConsoleDialect::Sql && is_comment_only(s))
        })
        .collect();

    let hit = spans
        .iter()
        .find(|r| offset >= r.start && offset <= r.end)
        // Otherwise the statement the caret sits BELOW — the one just
        // finished, which is what pressing run there means.
        .or_else(|| spans.iter().rev().find(|r| r.end <= offset))
        .or_else(|| spans.first())?;
    Some((hit.clone(), text[hit.clone()].to_string()))
}

/// `range` with leading and trailing whitespace removed, or `None` when it
/// holds nothing but whitespace.
fn trimmed_range(text: &str, range: std::ops::Range<usize>) -> Option<std::ops::Range<usize>> {
    let slice = &text[range.clone()];
    let lead = slice.len() - slice.trim_start().len();
    let trimmed = slice.trim();
    if trimmed.is_empty() {
        return None;
    }
    let start = range.start + lead;
    Some(start..start + trimmed.len())
}

/// Byte offset of a (row, column) caret in `text`, both zero-based and
/// counted in characters, as the console editor stores them.
pub fn offset_of(text: &str, row: usize, col: usize) -> usize {
    let mut offset = 0usize;
    for (i, line) in text.split('\n').enumerate() {
        if i == row {
            return offset
                + line
                    .char_indices()
                    .nth(col)
                    .map(|(b, _)| b)
                    .unwrap_or(line.len());
        }
        offset += line.len() + 1; // + the newline
    }
    text.len()
}

/// One Redis command per line, quote-aware: a newline inside an open quote
/// belongs to the argument, so a multi-line `EVAL "…lua…" 1 k` stays one
/// command. Blank and `#` comment lines are dropped by the caller.
///
/// Nothing is trimmed off the ends of a command: a `;` is part of the value
/// (`SET k a;b;c;` stores the trailing delimiter), matching the quoting rules
/// `parse_command_line` applies next.
fn command_line_spans(text: &str) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut quote: Option<char> = None;
    let mut chars = text.char_indices().peekable();

    while let Some((i, c)) = chars.next() {
        match c {
            // A backslash escapes the next character (including a newline)
            // wherever `parse_command_line` would honour it.
            '\\' if quote != Some('\'') => {
                chars.next();
            }
            '\'' | '"' if quote.is_none() => quote = Some(c),
            _ if Some(c) == quote => quote = None,
            '\n' if quote.is_none() => {
                out.push(start..i);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(start..text.len());
    out
}

/// Byte ranges of a run of JSON objects, one statement each, tracking brace
/// depth outside strings so `{"a": {"b": 1}}` stays whole and `{...} {...}`
/// splits. Text outside any object (a stray token) becomes its own range so
/// the driver reports the JSON error instead of it vanishing silently.
fn json_object_spans(text: &str) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    // Where the current statement began — the first character that is
    // neither whitespace nor a `;` separator.
    let mut start: Option<usize> = None;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for (i, c) in text.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                start.get_or_insert(i);
                in_string = true;
            }
            '{' => {
                start.get_or_insert(i);
                depth += 1;
            }
            '}' => {
                start.get_or_insert(i);
                depth = depth.saturating_sub(1);
                if depth == 0 && let Some(s) = start.take() {
                    out.push(s..i + c.len_utf8());
                }
            }
            // A separator between two objects is noise, not content.
            ';' if depth == 0 => {}
            _ if c.is_whitespace() && depth == 0 && start.is_none() => {}
            _ => {
                start.get_or_insert(i);
            }
        }
    }
    if let Some(s) = start {
        out.push(s..text.len());
    }
    out
}

/// Is `stmt` a pure comment — nothing but `--` line / `/* */` block comments
/// and whitespace? Such statements are skipped at execution. A statement like
/// `-- note\nSELECT 1` is NOT comment-only (the `SELECT` must still run).
pub fn is_comment_only(stmt: &str) -> bool {
    strip_comments(stmt).trim().is_empty()
}

/// Remove `--` line comments and `/* */` block comments, leaving string
/// literals, backtick identifiers and `$tag$` dollar-quotes untouched so a
/// `'--'` value or `$body$ -- x $body$` isn't mistaken for a comment.
fn strip_comments(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut in_string: Option<char> = None;
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = sql[i..].chars().next().unwrap();
        if in_line_comment {
            if c == '\n' {
                in_line_comment = false;
                out.push(c);
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
            out.push(c);
            if c == '\\' {
                if let Some(nc) = sql[i + 1..].chars().next() {
                    out.push(nc);
                    i += 1 + nc.len_utf8();
                } else {
                    i += 1;
                }
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
            out.push(c);
            i += 1;
            continue;
        }
        if c == '$' && let Some(end) = dollar_quote_end(sql, i) {
            out.push_str(&sql[i..end]);
            i = end;
            continue;
        }
        if c == '-' && bytes.get(i + 1) == Some(&b'-') {
            let after = sql[i + 2..].chars().next();
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
        out.push(c);
        i += c.len_utf8();
    }
    out
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_statements_basic() {
        assert_eq!(
            split_statements("SELECT 1; SELECT 2;"),
            vec!["SELECT 1".to_string(), "SELECT 2".to_string()]
        );
        // Semicolons inside string literals / comments must not split.
        let stmts = split_statements("SELECT 'a;b'; SELECT 2 -- ; done");
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[0], "SELECT 'a;b'");
        assert!(stmts[1].contains("SELECT 2"));
        // Block comments.
        let stmts = split_statements("SELECT 1 /* ; */; SELECT 3");
        assert_eq!(stmts, vec!["SELECT 1 /* ; */".to_string(), "SELECT 3".to_string()]);
        // Trailing semicolon ignored; empty input → no statements.
        assert!(split_statements(";;;").is_empty());
        assert!(split_statements("   ").is_empty());
    }

    #[test]
    fn test_split_statements_escaped_quotes() {
        // MySQL `\'` — the escaped quote must not close the string.
        let stmts = split_statements(r"SELECT 'it\'s'; SELECT 2");
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[0], r"SELECT 'it\'s'");
    }

    #[test]
    fn test_split_statements_backtick_identifier() {
        // `a;b` is a MySQL identifier — the `;` inside must not split.
        let stmts = split_statements("SELECT `a;b` FROM t; SELECT 2");
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[0], "SELECT `a;b` FROM t");
    }

    #[test]
    fn test_split_statements_dollar_quote() {
        // A `;` inside a PostgreSQL `$$…$$` / `$body$…$body$` must not split.
        let stmts = split_statements("CREATE FUNCTION f() RETURNS void AS $$ BEGIN; END; $$ LANGUAGE plpgsql; SELECT 2");
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].contains("$$ BEGIN; END; $$"));

        let stmts = split_statements("SELECT $tag$ a; b $tag$; SELECT 2");
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[0], "SELECT $tag$ a; b $tag$");

        // A bare `$` (not a dollar-quote) is left alone.
        let stmts = split_statements("SELECT $1; SELECT 2");
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[0], "SELECT $1");
    }

    #[test]
    fn test_split_statements_double_dash_needs_whitespace() {
        // `a--b` is valid MySQL arithmetic (a - (-b)), not a comment.
        let stmts = split_statements("SELECT a--b FROM t; SELECT 2");
        assert_eq!(stmts.len(), 2);
        // But `-- ` IS a comment.
        let stmts = split_statements("SELECT 1; -- done");
        assert_eq!(stmts, vec!["SELECT 1".to_string(), "-- done".to_string()]);
        assert!(is_comment_only("-- done"));
        assert!(!is_comment_only("SELECT 1"));
        assert!(is_comment_only("/* just a note */"));
        assert!(!is_comment_only("/* note */ SELECT 1"));
    }

    #[test]
    fn test_is_comment_only() {
        // Pure comments are skipped.
        assert!(is_comment_only("-- just a note"));
        assert!(is_comment_only("/* whole block */"));
        assert!(is_comment_only("  -- leading ws\n-- more"));
        // A statement after a leading comment still runs.
        assert!(!is_comment_only("-- note\nSELECT 1"));
        assert!(!is_comment_only("/* lead */ SELECT 1"));
        assert!(!is_comment_only("SELECT 1"));
        assert!(!is_comment_only("'-- not a comment'"));
    }

    #[test]
    fn test_split_statements_for_redis_is_line_based() {
        let out = split_statements_for(
            ConsoleDialect::RedisCommand,
            "# warm up\nSCAN 0 MATCH user:*\n\nSET k \"a;b\"\nSET csv a;b;c;",
        );
        assert_eq!(
            out,
            vec![
                "SCAN 0 MATCH user:*".to_string(),
                // A `;` is data, not a separator — quoted or not, and a
                // trailing one is part of the value.
                "SET k \"a;b\"".to_string(),
                "SET csv a;b;c;".to_string(),
            ]
        );
    }

    #[test]
    fn test_split_statements_for_redis_keeps_multiline_quoted_arguments() {
        // A Lua script spans lines; cutting on the raw newline would hand
        // `EVAL "` to the driver and abort the whole run.
        let script = "EVAL \"\n  return redis.call('GET', KEYS[1])\n\" 1 mykey\nGET other";
        let out = split_statements_for(ConsoleDialect::RedisCommand, script);
        assert_eq!(out.len(), 2, "got {out:?}");
        assert!(out[0].starts_with("EVAL \""), "got {out:?}");
        assert!(out[0].ends_with("1 mykey"), "got {out:?}");
        assert_eq!(out[1], "GET other");
    }

    #[test]
    fn test_split_statements_for_redis_honours_escapes() {
        // A backslash-escaped newline keeps the command together, and an
        // escaped quote does not open a quoted run.
        let out = split_statements_for(ConsoleDialect::RedisCommand, "SET k a\\\nb\nGET k");
        assert_eq!(out.len(), 2, "got {out:?}");
        assert_eq!(out[1], "GET k");
    }

    #[test]
    fn test_split_statements_for_mongo_splits_json_objects() {
        let out = split_statements_for(
            ConsoleDialect::MongoJson,
            "{\"collection\": \"users\", \"find\": {\"filter\": {\"age\": {\"$gte\": 18}}}}\n             {\"collection\": \"logs\", \"aggregate\": []}",
        );
        assert_eq!(out.len(), 2, "two commands, got {out:?}");
        assert!(out[0].ends_with("}}}}"), "nested braces broken: {out:?}");
        assert!(out[1].contains("\"logs\""));
    }

    #[test]
    fn test_split_statements_for_mongo_keeps_braces_inside_strings() {
        // A `}` or `;` inside a JSON string must not end the object.
        let src = r#"{"collection": "u", "find": {"filter": {"note": "a} b; c"}}}"#;
        assert_eq!(
            split_statements_for(ConsoleDialect::MongoJson, src),
            vec![src.to_string()]
        );
        // An escaped quote does not reopen the string either.
        let esc = r#"{"collection": "u", "find": {"filter": {"q": "say \"hi\"}"}}}"#;
        assert_eq!(
            split_statements_for(ConsoleDialect::MongoJson, esc),
            vec![esc.to_string()]
        );
    }

    #[test]
    fn test_split_statements_for_sql_is_unchanged() {
        assert_eq!(
            split_statements_for(ConsoleDialect::Sql, "SELECT 1; SELECT 2;"),
            vec!["SELECT 1".to_string(), "SELECT 2".to_string()]
        );
    }
    #[test]
    fn test_statement_at_picks_the_one_under_the_caret() {
        let sql = "SELECT 1;\nSELECT 2;\nDELETE FROM logs;";
        // Caret inside the second statement.
        let off = offset_of(sql, 1, 3);
        let (_, stmt) = statement_at(ConsoleDialect::Sql, sql, off).expect("a statement");
        assert_eq!(stmt, "SELECT 2");

        // Caret on the third line: the DELETE, and nothing else, would run.
        let off = offset_of(sql, 2, 0);
        let (_, stmt) = statement_at(ConsoleDialect::Sql, sql, off).unwrap();
        assert_eq!(stmt, "DELETE FROM logs");
    }

    #[test]
    fn test_statement_at_takes_the_one_above_a_blank_caret() {
        // Parking on the empty line under a query and pressing run means
        // "run that query", not "run nothing".
        let sql = "SELECT 1;\n\n";
        let off = offset_of(sql, 2, 0);
        let (_, stmt) = statement_at(ConsoleDialect::Sql, sql, off).unwrap();
        assert_eq!(stmt, "SELECT 1");

        // Before the first statement, take the first.
        let sql = "\n\nSELECT 9";
        let (_, stmt) = statement_at(ConsoleDialect::Sql, sql, 0).unwrap();
        assert_eq!(stmt, "SELECT 9");

        // An empty buffer has nothing to run.
        assert!(statement_at(ConsoleDialect::Sql, "   \n", 0).is_none());
    }

    #[test]
    fn test_statement_at_respects_strings_and_comments() {
        let sql = "SELECT 'a;b' AS x;\nSELECT 2";
        let off = offset_of(sql, 0, 12);
        let (_, stmt) = statement_at(ConsoleDialect::Sql, sql, off).unwrap();
        assert_eq!(stmt, "SELECT 'a;b' AS x", "a `;` in a literal is not a split");

        let sql = "-- note; still\nSELECT 1;\nSELECT 2";
        let off = offset_of(sql, 1, 2);
        let (_, stmt) = statement_at(ConsoleDialect::Sql, sql, off).unwrap();
        assert_eq!(stmt, "-- note; still\nSELECT 1");
    }

    #[test]
    fn test_statement_at_follows_the_dialect() {
        // Redis: one command per line.
        let text = "SET a 1\nGET a\nDEL a";
        let off = offset_of(text, 1, 1);
        let (_, stmt) = statement_at(ConsoleDialect::RedisCommand, text, off).unwrap();
        assert_eq!(stmt, "GET a");

        // Mongo: the JSON object the caret is inside.
        let text = "{\"collection\": \"a\", \"find\": {}}\n{\"collection\": \"b\", \"find\": {}}";
        let off = offset_of(text, 1, 5);
        let (_, stmt) = statement_at(ConsoleDialect::MongoJson, text, off).unwrap();
        assert!(stmt.contains("\"b\""), "got {stmt}");
    }

    #[test]
    fn test_offset_of_handles_multibyte_and_out_of_range() {
        let text = "héllo\nwörld";
        // Column 2 of line 0 is past a two-byte character.
        assert_eq!(offset_of(text, 0, 2), 3);
        // Past the end of a line clamps to its end, not into the next line.
        assert_eq!(offset_of(text, 0, 99), 6);
        // Past the last row clamps to the end of the text.
        assert_eq!(offset_of(text, 99, 0), text.len());
    }
    #[test]
    fn test_statement_at_never_reaches_past_the_caret() {
        // A span starts right after the previous `;`, so the blank line under
        // a statement lives inside the NEXT span. Picking by raw span made a
        // caret parked below `SELECT …;` run the UPDATE beneath it.
        let sql = "SELECT * FROM users;\n\nUPDATE users SET active = 0 WHERE id = 1;";
        for (row, col) in [(0usize, 20usize), (1, 0)] {
            let off = offset_of(sql, row, col);
            let (_, stmt) = statement_at(ConsoleDialect::Sql, sql, off).unwrap();
            assert_eq!(
                stmt, "SELECT * FROM users",
                "caret at row {row} col {col} reached the statement below it"
            );
        }
        // On the UPDATE's own line it does run the UPDATE.
        let off = offset_of(sql, 2, 3);
        let (_, stmt) = statement_at(ConsoleDialect::Sql, sql, off).unwrap();
        assert!(stmt.starts_with("UPDATE"), "got {stmt}");
    }

    #[test]
    fn test_statement_at_skips_a_trailing_comment() {
        // Leaving the caret on a note under a query is normal; running it
        // must re-run the query, not report "empty query" and wipe the grid.
        let sql = "SELECT * FROM orders;\n-- TODO: add WHERE";
        let off = offset_of(sql, 1, 5);
        let (_, stmt) = statement_at(ConsoleDialect::Sql, sql, off).unwrap();
        assert_eq!(stmt, "SELECT * FROM orders");
    }
}

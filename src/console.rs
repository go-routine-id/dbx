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

/// Split a query into individual `;`-separated statements, ignoring `;`
/// inside string literals (incl. backslash-escaped quotes), backtick
/// identifiers, `--` line comments (only when followed by whitespace, per
/// the SQL standard / MySQL), `/* */` block comments, and PostgreSQL
/// `$tag$` dollar-quoted bodies. Returns trimmed, non-empty statements.
pub fn split_statements(sql: &str) -> Vec<String> {
    let mut stmts = Vec::new();
    let mut cur = String::new();
    let mut in_string: Option<char> = None;
    let mut in_line_comment = false;
    let mut in_block_comment = false;

    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = sql[i..].chars().next().unwrap();
        if in_line_comment {
            cur.push(c);
            if c == '\n' {
                in_line_comment = false;
            }
            i += c.len_utf8();
            continue;
        }
        if in_block_comment {
            cur.push(c);
            if c == '*' && bytes.get(i + 1) == Some(&b'/') {
                cur.push('/');
                in_block_comment = false;
                i += 2;
            } else {
                i += c.len_utf8();
            }
            continue;
        }
        if let Some(q) = in_string {
            cur.push(c);
            // Backslash escape keeps the next char from closing the string
            // (MySQL `\'`). Consume it so it isn't re-scanned.
            if c == '\\' {
                if let Some(nc) = sql[i + 1..].chars().next() {
                    cur.push(nc);
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
            cur.push(c);
            i += 1;
            continue;
        }
        // PostgreSQL dollar-quote: a `;` inside `$$…$$` / `$tag$…$tag$` is
        // part of the body and must not split the statement.
        if c == '$' && let Some(end) = dollar_quote_end(sql, i) {
            cur.push_str(&sql[i..end]);
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
                cur.push('-');
                cur.push('-');
                i += 2;
                continue;
            }
        }
        if c == '/' && bytes.get(i + 1) == Some(&b'*') {
            in_block_comment = true;
            cur.push('/');
            cur.push('*');
            i += 2;
            continue;
        }
        if c == ';' {
            let s = cur.trim();
            if !s.is_empty() {
                stmts.push(s.to_string());
            }
            cur.clear();
            i += 1;
            continue;
        }
        cur.push(c);
        i += c.len_utf8();
    }
    let tail = cur.trim();
    if !tail.is_empty() {
        stmts.push(tail.to_string());
    }
    stmts
}

/// Split console text into statements the way the target server reads it.
///
/// Only SQL uses `;`. Redis takes one command per line, and MongoDB one JSON
/// object per command — feeding either through the SQL splitter merged a
/// whole script into a single unparseable statement.
pub fn split_statements_for(dialect: ConsoleDialect, text: &str) -> Vec<String> {
    match dialect {
        ConsoleDialect::Sql => split_statements(text),
        ConsoleDialect::RedisCommand => split_command_lines(text),
        ConsoleDialect::MongoJson => split_json_objects(text),
    }
}

/// One Redis command per line, quote-aware: a newline inside an open quote
/// belongs to the argument, so a multi-line `EVAL "…lua…" 1 k` stays one
/// command. Blank lines and `#` comment lines are dropped.
///
/// Nothing is trimmed off the ends of a command: a `;` is part of the value
/// (`SET k a;b;c;` stores the trailing delimiter), matching the quoting rules
/// `parse_command_line` applies next.
fn split_command_lines(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut chars = text.chars().peekable();

    let push = |cur: &mut String, out: &mut Vec<String>| {
        let line = cur.trim();
        if !line.is_empty() && !line.starts_with('#') {
            out.push(line.to_string());
        }
        cur.clear();
    };

    while let Some(c) = chars.next() {
        match c {
            // A backslash escapes the next character (including a newline)
            // wherever `parse_command_line` would honour it.
            '\\' if quote != Some('\'') => {
                cur.push(c);
                if let Some(next) = chars.next() {
                    cur.push(next);
                }
            }
            '\'' | '"' if quote.is_none() => {
                quote = Some(c);
                cur.push(c);
            }
            _ if Some(c) == quote => {
                quote = None;
                cur.push(c);
            }
            '\n' if quote.is_none() => push(&mut cur, &mut out),
            _ => cur.push(c),
        }
    }
    push(&mut cur, &mut out);
    out
}

/// Split a run of JSON objects into one statement each, tracking brace depth
/// outside strings so `{"a": {"b": 1}}` stays whole and `{...} {...}` splits.
/// Text outside any object (a stray token) is kept as its own statement so
/// the driver reports the JSON error instead of it vanishing silently.
fn split_json_objects(text: &str) -> Vec<String> {
    let mut stmts = Vec::new();
    let mut cur = String::new();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for c in text.chars() {
        if in_string {
            cur.push(c);
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
                in_string = true;
                cur.push(c);
            }
            '{' => {
                depth += 1;
                cur.push(c);
            }
            '}' => {
                depth = depth.saturating_sub(1);
                cur.push(c);
                if depth == 0 {
                    let s = cur.trim();
                    if !s.is_empty() {
                        stmts.push(s.to_string());
                    }
                    cur.clear();
                }
            }
            // A separator between two objects is noise, not content.
            ';' if depth == 0 => {}
            _ => cur.push(c),
        }
    }
    let tail = cur.trim();
    if !tail.is_empty() {
        stmts.push(tail.to_string());
    }
    stmts
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
}

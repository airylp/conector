//! SQL de la consola: separar un script en sentencias, encontrar la sentencia
//! bajo el cursor y colorear (sin dependencias; tolerante con cualquier dialecto).
//!
//! Separadores: `;` fuera de cadenas/comentarios, una línea con solo `/`
//! (Oracle) o `GO` (SQL Server). Los bloques PL/SQL (`BEGIN`, `DECLARE`,
//! `CREATE … PROCEDURE/FUNCTION/TRIGGER/PACKAGE/TYPE`) solo terminan en `/`
//! o `GO`, porque llevan `;` dentro. Respeta `'…'`, `"…"`, `` `…` ``, `[…]`,
//! `-- …`, `# …` no, `/* … */` y `$$ … $$` / `$tag$ … $tag$` (PostgreSQL).

use std::ops::Range;

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Code,
    Single,
    Double,
    Back,
    LineComment,
    BlockComment,
}

/// ¿La sentencia empieza como un bloque procedimental?
fn is_block(text: &str) -> bool {
    let words: Vec<String> = strip_leading_comments(text).split_whitespace().take(6).map(|w| w.to_ascii_uppercase()).collect();
    let w = |i: usize| words.get(i).map(String::as_str).unwrap_or("");
    if matches!(w(0), "BEGIN" | "DECLARE") {
        // `BEGIN TRANSACTION`/`BEGIN;` de otras bases no es un bloque.
        return !(w(0) == "BEGIN" && (w(1).starts_with("TRAN") || w(1).starts_with("WORK") || w(1).is_empty() || w(1) == ";"));
    }
    if w(0) != "CREATE" {
        return false;
    }
    let mut i = 1;
    if w(i) == "OR" && w(i + 1) == "REPLACE" {
        i += 2;
    }
    if matches!(w(i), "EDITIONABLE" | "NONEDITIONABLE") {
        i += 1;
    }
    matches!(w(i), "PROCEDURE" | "FUNCTION" | "TRIGGER" | "PACKAGE" | "TYPE")
}

pub fn strip_leading_comments(mut s: &str) -> &str {
    loop {
        s = s.trim_start();
        if let Some(r) = s.strip_prefix("--") {
            s = r.split_once('\n').map_or("", |x| x.1);
        } else if let Some(r) = s.strip_prefix("/*") {
            s = r.split_once("*/").map_or("", |x| x.1);
        } else {
            return s;
        }
    }
}

/// Línea que es solo un separador (`/` o `GO`)?
fn separator_line(line: &str) -> bool {
    let t = line.trim();
    t == "/" || t.eq_ignore_ascii_case("go")
}

/// Rangos (en bytes) de cada sentencia, sin el separador ni espacios alrededor.
/// Las que solo tienen comentarios se descartan.
pub fn split(text: &str) -> Vec<Range<usize>> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut start = 0;
    let mut mode = Mode::Code;
    let mut dollar: Option<&str> = None;
    // El cuerpo ya fue entre $$ (PostgreSQL): el «;» sí termina la sentencia.
    let mut saw_dollar = false;
    let mut i = 0;
    let mut line_start = true;
    let push = |out: &mut Vec<Range<usize>>, s: usize, e: usize| {
        let piece = &text[s..e];
        if !strip_leading_comments(piece).is_empty() {
            let lead = piece.len() - piece.trim_start().len();
            let trail = piece.len() - piece.trim_end().len();
            out.push(s + lead..e - trail);
        }
    };
    while i < b.len() {
        let c = b[i];
        // Separador de línea completa (fuera de cadenas y comentarios de bloque).
        if line_start && matches!(mode, Mode::Code) && dollar.is_none() {
            let end = text[i..].find('\n').map_or(b.len(), |n| i + n);
            if separator_line(&text[i..end]) {
                push(&mut out, start, i);
                i = (end + 1).min(b.len());
                start = i;
                saw_dollar = false;
                continue;
            }
        }
        line_start = c == b'\n';
        if let Some(tag) = dollar {
            if text[i..].starts_with(tag) {
                i += tag.len();
                dollar = None;
                continue;
            }
            i += 1;
            continue;
        }
        match mode {
            Mode::Code => match c {
                b'\'' => mode = Mode::Single,
                b'"' => mode = Mode::Double,
                b'`' => mode = Mode::Back,
                b'-' if b.get(i + 1) == Some(&b'-') => mode = Mode::LineComment,
                b'/' if b.get(i + 1) == Some(&b'*') => {
                    mode = Mode::BlockComment;
                    i += 1;
                }
                b'$' => {
                    // $$ o $etiqueta$
                    let rest = &text[i + 1..];
                    if let Some(n) = rest.find('$') {
                        let tag = &rest[..n];
                        if tag.chars().all(|c| c.is_alphanumeric() || c == '_') && !tag.starts_with(|c: char| c.is_ascii_digit()) {
                            let full = &text[i..i + n + 2];
                            dollar = Some(full);
                            saw_dollar = true;
                            i += full.len();
                            continue;
                        }
                    }
                }
                b';' => {
                    if saw_dollar || !is_block(&text[start..i]) {
                        push(&mut out, start, i);
                        start = i + 1;
                        saw_dollar = false;
                    }
                }
                _ => {}
            },
            Mode::Single if c == b'\'' => mode = Mode::Code, // '' se cierra y reabre: da igual
            Mode::Double if c == b'"' => mode = Mode::Code,
            Mode::Back if c == b'`' => mode = Mode::Code,
            Mode::LineComment if c == b'\n' => mode = Mode::Code,
            Mode::BlockComment if c == b'*' && b.get(i + 1) == Some(&b'/') => {
                mode = Mode::Code;
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }
    push(&mut out, start, b.len());
    // Un bloque PL/SQL sin «/» final se envía con su último «;» (lo exige Oracle).
    out
}

/// Sentencia que contiene la posición `at` (byte); si cae entre dos, la anterior.
pub fn at_cursor(text: &str, at: usize) -> Option<Range<usize>> {
    let parts = split(text);
    parts.iter().find(|r| r.start <= at && at <= r.end).or_else(|| parts.iter().rev().find(|r| r.end <= at)).or(parts.first()).cloned()
}

/// Texto listo para JDBC: sin el «;» final, salvo en bloques PL/SQL (que lo necesitan).
pub fn for_jdbc(stmt: &str) -> String {
    let s = stmt.trim();
    if is_block(s) {
        if s.ends_with(';') { s.to_owned() } else { format!("{s};") }
    } else {
        s.trim_end_matches(';').trim_end().to_owned()
    }
}

// ---------- coloreado ----------

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Tok {
    Plain,
    Keyword,
    Str,
    Number,
    Comment,
}

const KEYWORDS: &[&str] = &[
    "ADD", "ALL", "ALTER", "AND", "ANY", "AS", "ASC", "BEGIN", "BETWEEN", "BY", "CASE", "CAST", "CHECK", "COLUMN", "COMMIT", "CONSTRAINT", "CREATE", "CROSS", "DATABASE", "DECLARE", "DEFAULT", "DELETE", "DESC", "DISTINCT",
    "DROP", "ELSE", "END", "EXCEPT", "EXEC", "EXISTS", "EXPLAIN", "FETCH", "FIRST", "FOR", "FOREIGN", "FROM", "FULL", "FUNCTION", "GRANT", "GROUP", "HAVING", "IF", "IN", "INDEX", "INNER", "INSERT", "INTERSECT", "INTO", "IS", "JOIN",
    "KEY", "LEFT", "LIKE", "LIMIT", "MERGE", "NOT", "NULL", "OFFSET", "ON", "OR", "ORDER", "OUTER", "OVER", "PARTITION", "PRIMARY", "PROCEDURE", "REFERENCES", "REPLACE", "RETURN", "RETURNING", "REVOKE", "RIGHT", "ROLLBACK",
    "ROWNUM", "ROWS", "SCHEMA", "SELECT", "SET", "TABLE", "THEN", "TOP", "TRIGGER", "TRUNCATE", "UNION", "UNIQUE", "UPDATE", "USING", "VALUES", "VIEW", "WHEN", "WHERE", "WITH",
];

/// Trozos (rango en bytes, tipo) para colorear todo el texto.
pub fn tokens(text: &str) -> Vec<(Range<usize>, Tok)> {
    let b = text.as_bytes();
    let mut out: Vec<(Range<usize>, Tok)> = Vec::new();
    let mut push = |r: Range<usize>, t: Tok| {
        if let Some(last) = out.last_mut() {
            if last.1 == t && last.0.end == r.start {
                last.0.end = r.end;
                return;
            }
        }
        out.push((r, t));
    };
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let start = i;
        let tok = if c == b'-' && b.get(i + 1) == Some(&b'-') {
            i = text[i..].find('\n').map_or(b.len(), |n| i + n);
            Tok::Comment
        } else if c == b'/' && b.get(i + 1) == Some(&b'*') {
            i = text[i + 2..].find("*/").map_or(b.len(), |n| i + 2 + n + 2);
            Tok::Comment
        } else if c == b'\'' {
            i += 1;
            while i < b.len() && b[i] != b'\'' {
                i += 1;
            }
            i = (i + 1).min(b.len());
            Tok::Str
        } else if c.is_ascii_digit() && (i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_')) {
            while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
                i += 1;
            }
            Tok::Number
        } else if c.is_ascii_alphabetic() || c == b'_' {
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            let w = &text[start..i];
            if KEYWORDS.iter().any(|k| k.eq_ignore_ascii_case(w)) { Tok::Keyword } else { Tok::Plain }
        } else {
            // Un carácter (respetando UTF-8).
            i += text[i..].chars().next().map_or(1, char::len_utf8);
            Tok::Plain
        };
        push(start..i, tok);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(t: &str) -> Vec<&str> {
        split(t).into_iter().map(|r| &t[r]).collect()
    }

    #[test]
    fn splits_respecting_strings_and_comments() {
        let t = "select 'a;b' from t; -- c;\nselect \"x;\" /* ; */ from u ;\n\n-- solo comentario;\n";
        assert_eq!(parts(t), vec!["select 'a;b' from t", "-- c;\nselect \"x;\" /* ; */ from u"]);
        assert_eq!(parts("select 1"), vec!["select 1"]);
        assert_eq!(parts(" ; ;select 2;"), vec!["select 2"]);
    }

    #[test]
    fn plsql_blocks_and_separators() {
        let t = "create or replace procedure p as\nbegin\n  null;\nend;\n/\nselect 1 from dual;\nbegin\n x := 1;\nend;\n/\n";
        let p = parts(t);
        assert_eq!(p.len(), 3);
        assert!(p[0].starts_with("create or replace procedure") && p[0].ends_with("end;"));
        assert_eq!(p[1], "select 1 from dual");
        assert_eq!(for_jdbc(p[0]), p[0]);
        assert_eq!(for_jdbc("select 1;"), "select 1");
        // SQL Server: GO.
        assert_eq!(parts("select 1\nGO\nselect 2\ngo"), vec!["select 1", "select 2"]);
        // BEGIN TRANSACTION no es un bloque.
        assert_eq!(parts("begin transaction; update t set a=1; commit;"), vec!["begin transaction", "update t set a=1", "commit"]);
    }

    #[test]
    fn postgres_dollar_quotes() {
        let t = "create function f() returns int as $$ begin return 1; end; $$ language plpgsql; select f();";
        assert_eq!(parts(t), vec!["create function f() returns int as $$ begin return 1; end; $$ language plpgsql", "select f()"]);
    }

    #[test]
    fn statement_under_cursor() {
        let t = "select 1;\nselect 2;\n\nselect 3";
        assert_eq!(&t[at_cursor(t, 12).unwrap()], "select 2");
        assert_eq!(&t[at_cursor(t, 0).unwrap()], "select 1");
        assert_eq!(&t[at_cursor(t, 20).unwrap()], "select 2"); // en la línea vacía: la anterior
        assert_eq!(&t[at_cursor(t, t.len()).unwrap()], "select 3");
    }

    #[test]
    fn colors() {
        let t = "SELECT 'x', 12 -- fin\nfrom ñ";
        let k: Vec<(&str, Tok)> = tokens(t).into_iter().map(|(r, k)| (&t[r], k)).collect();
        assert_eq!(k[0], ("SELECT", Tok::Keyword));
        assert!(k.contains(&("'x'", Tok::Str)));
        assert!(k.contains(&("12", Tok::Number)));
        assert!(k.contains(&("-- fin", Tok::Comment)));
        assert!(k.contains(&("from", Tok::Keyword)));
    }
}

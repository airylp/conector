//! Protocolo de línea con el auxiliar de Java: campos separados por TAB,
//! escapados con `\\`, `\t`, `\n`, `\r`; el campo `\N` es NULL.
//! (Mismo formato en `java/ConectorHelper.java`.)

/// Añade `s` escapado a `out`.
pub fn escape_into(out: &mut String, s: &str) {
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
}

/// Arma una línea `orden \t id \t clave=valor ...` (sin el salto final).
pub fn line(cmd: &str, id: u64, fields: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(64);
    out.push_str(cmd);
    out.push('\t');
    out.push_str(&id.to_string());
    for (k, v) in fields {
        out.push('\t');
        out.push_str(k);
        out.push('=');
        escape_into(&mut out, v);
    }
    out
}

/// Separa una línea en campos; `None` = NULL.
pub fn split(line: &str) -> Vec<Option<Box<str>>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut null = false;
    let mut it = line.chars();
    while let Some(ch) = it.next() {
        match ch {
            '\t' => {
                out.push(if null && cur.is_empty() { None } else { Some(cur.as_str().into()) });
                cur.clear();
                null = false;
            }
            '\\' => match it.next() {
                Some('t') => cur.push('\t'),
                Some('n') => cur.push('\n'),
                Some('r') => cur.push('\r'),
                Some('N') => null = true,
                Some(c) => cur.push(c),
                None => cur.push('\\'),
            },
            c => cur.push(c),
        }
    }
    out.push(if null && cur.is_empty() { None } else { Some(cur.into()) });
    out
}

/// Un mensaje del auxiliar: `tipo \t id \t campos...`.
#[derive(Debug, Clone)]
pub struct Msg {
    pub kind: Box<str>,
    pub id: u64,
    pub fields: Vec<Option<Box<str>>>,
}

impl Msg {
    pub fn parse(line: &str) -> Option<Msg> {
        let mut f = split(line);
        if f.len() < 2 {
            return None;
        }
        let rest = f.split_off(2);
        let id = f[1].as_deref()?.parse().ok()?;
        Some(Msg { kind: f[0].take()?, id, fields: rest })
    }

    /// Valor de un campo `clave=valor`.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.fields.iter().flatten().find_map(|f| f.strip_prefix(key)?.strip_prefix('='))
    }

    /// Todos los `clave=valor` en orden.
    pub fn pairs(&self) -> impl Iterator<Item = (&str, &str)> {
        self.fields.iter().flatten().filter_map(|f| f.split_once('='))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_escapes() {
        let raw = "a\tb\\c\nd\re=f";
        let l = line("consulta", 7, &[("sql", raw), ("max", "10")]);
        assert!(!l.contains('\n'));
        let m = Msg::parse(&l).unwrap();
        assert_eq!(&*m.kind, "consulta");
        assert_eq!(m.id, 7);
        assert_eq!(m.get("sql"), Some(raw));
        assert_eq!(m.get("max"), Some("10"));
    }

    #[test]
    fn null_fields() {
        let f = split("fila\t3\t\\N\t\t\\\\N");
        assert_eq!(f[2], None);
        assert_eq!(f[3].as_deref(), Some(""));
        assert_eq!(f[4].as_deref(), Some("\\N"));
    }
}

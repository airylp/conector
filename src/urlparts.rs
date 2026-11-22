//! URL JDBC ↔ campos (servidor, puerto, base de datos) para no escribir la URL a mano.
//!
//! `parse` reconoce las formas habituales de cada plantilla; lo que no entiende
//! (Oracle TNS, varios servidores, balanceo…) devuelve `None` y se edita la URL.
//! Lo que no se edita por campos (parámetros, propiedades) va tal cual en `rest`,
//! así `build(parse(url))` devuelve la misma URL en las formas normales.

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    /// `prefijo//servidor[:puerto]/base[resto]` (PostgreSQL, MySQL, MariaDB, DB2, Firebird, H2 tcp).
    Net,
    /// `jdbc:sqlserver://servidor[:puerto][;databaseName=base][;resto]`.
    SqlServer,
    /// `jdbc:oracle:thin:@//servidor[:puerto]/servicio`.
    OracleService,
    /// `jdbc:oracle:thin:@servidor:puerto:SID`.
    OracleSid,
    /// `prefijo` + ruta/nombre (SQLite, H2 en archivo o memoria).
    File,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Parts {
    pub kind: Kind,
    pub prefix: String,
    pub host: String,
    pub port: String,
    pub db: String,
    /// Parámetros/propiedades que siguen a la base, con su separador inicial.
    pub rest: String,
}

/// Prefijos de red reconocidos y su puerto por defecto.
const NET: &[(&str, &str)] = &[
    ("jdbc:postgresql:", "5432"),
    ("jdbc:mysql:", "3306"),
    ("jdbc:mariadb:", "3306"),
    ("jdbc:db2:", "50000"),
    ("jdbc:firebirdsql:", "3050"),
    ("jdbc:h2:tcp:", "9092"),
    ("jdbc:h2:ssl:", "9092"),
];

/// Puerto por defecto (para la pista del campo).
pub fn default_port(p: &Parts) -> &'static str {
    match p.kind {
        Kind::SqlServer => "1433",
        Kind::OracleService | Kind::OracleSid => "1521",
        Kind::File => "",
        Kind::Net => NET.iter().find(|(x, _)| *x == p.prefix).map_or("", |(_, d)| d),
    }
}

/// Posición del último `:` fuera de `${…}` y de `[…]` (IPv6).
fn last_colon(s: &str) -> Option<usize> {
    let (mut brace, mut bracket, mut found) = (0i32, 0i32, None);
    let b = s.as_bytes();
    for (i, &c) in b.iter().enumerate() {
        match c {
            b'{' if i > 0 && b[i - 1] == b'$' => brace += 1,
            b'}' if brace > 0 => brace -= 1,
            b'[' => bracket += 1,
            b']' if bracket > 0 => bracket -= 1,
            b':' if brace == 0 && bracket == 0 => found = Some(i),
            _ => {}
        }
    }
    found
}

/// Primera aparición de alguno de `chars` fuera de `${…}`.
fn find_outside(s: &str, chars: &[u8]) -> Option<usize> {
    let b = s.as_bytes();
    let mut brace = 0;
    for (i, &c) in b.iter().enumerate() {
        match c {
            b'{' if i > 0 && b[i - 1] == b'$' => brace += 1,
            b'}' if brace > 0 => brace -= 1,
            c if brace == 0 && chars.contains(&c) => return Some(i),
            _ => {}
        }
    }
    None
}

fn is_port(s: &str) -> bool {
    !s.is_empty() && (s.bytes().all(|c| c.is_ascii_digit()) || (s.starts_with("${") && s.ends_with('}')))
}

/// `servidor[:puerto]` → (servidor, puerto).
fn split_host(a: &str) -> Option<(String, String)> {
    if a.contains(',') || a.contains('@') || a.contains('/') {
        return None; // varios servidores o credenciales en la URL
    }
    match last_colon(a) {
        Some(i) if is_port(&a[i + 1..]) => Some((a[..i].to_owned(), a[i + 1..].to_owned())),
        Some(_) if !a.starts_with('[') => None,
        _ => Some((a.to_owned(), String::new())),
    }
}

fn split_rest(s: &str, seps: &[u8]) -> (String, String) {
    match find_outside(s, seps) {
        Some(i) => (s[..i].to_owned(), s[i..].to_owned()),
        None => (s.to_owned(), String::new()),
    }
}

pub fn parse(url: &str) -> Option<Parts> {
    let url = url.trim();
    let mk = |kind, prefix: &str, host: String, port: String, db: String, rest: String| Some(Parts { kind, prefix: prefix.to_owned(), host, port, db, rest });
    for (prefix, _) in NET {
        let Some(after) = url.strip_prefix(prefix) else { continue };
        let after = after.strip_prefix("//")?;
        let (auth, path) = match after.find('/') {
            Some(i) => (&after[..i], &after[i + 1..]),
            None => (after, ""),
        };
        let (host, port) = split_host(auth)?;
        // DB2 pone las propiedades tras «:»; H2 tras «;»; el resto con «?».
        let seps: &[u8] = match *prefix {
            "jdbc:db2:" => b":?",
            "jdbc:h2:tcp:" | "jdbc:h2:ssl:" => b";",
            _ => b"?",
        };
        let (db, rest) = split_rest(path, seps);
        return mk(Kind::Net, prefix, host, port, db, rest);
    }
    if let Some(after) = url.strip_prefix("jdbc:sqlserver:") {
        let after = after.strip_prefix("//")?;
        let (auth, props) = split_rest(after, b";");
        let (host, port) = split_host(&auth)?;
        let mut db = String::new();
        let mut rest = String::new();
        for p in props.split(';').filter(|p| !p.is_empty()) {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            if db.is_empty() && (k.eq_ignore_ascii_case("databaseName") || k.eq_ignore_ascii_case("database")) {
                db = v.to_owned();
            } else {
                rest.push(';');
                rest.push_str(p);
            }
        }
        return mk(Kind::SqlServer, "jdbc:sqlserver:", host, port, db, rest);
    }
    if let Some(after) = url.strip_prefix("jdbc:oracle:thin:@") {
        if let Some(a) = after.strip_prefix("//") {
            let i = a.find('/')?;
            let (host, port) = split_host(&a[..i])?;
            let (db, rest) = split_rest(&a[i + 1..], b"?");
            return mk(Kind::OracleService, "jdbc:oracle:thin:@", host, port, db, rest);
        }
        if after.starts_with('(') {
            return None; // descriptor TNS
        }
        let (main, rest) = split_rest(after, b"?");
        if let Some(i) = main.find('/') {
            // EZConnect sin «//»: se trata como servicio.
            let (host, port) = split_host(&main[..i])?;
            return mk(Kind::OracleService, "jdbc:oracle:thin:@", host, port, main[i + 1..].to_owned(), rest);
        }
        let i = last_colon(&main)?;
        let (host, port) = split_host(&main[..i])?;
        return mk(Kind::OracleSid, "jdbc:oracle:thin:@", host, port, main[i + 1..].to_owned(), rest);
    }
    if let Some(after) = url.strip_prefix("jdbc:sqlite:") {
        let (db, rest) = split_rest(after, b"?");
        return mk(Kind::File, "jdbc:sqlite:", String::new(), String::new(), db, rest);
    }
    if let Some(after) = url.strip_prefix("jdbc:h2:") {
        let (db, rest) = split_rest(after, b";");
        return mk(Kind::File, "jdbc:h2:", String::new(), String::new(), db, rest);
    }
    None
}

pub fn build(p: &Parts) -> String {
    let hp = if p.port.trim().is_empty() { p.host.trim().to_owned() } else { format!("{}:{}", p.host.trim(), p.port.trim()) };
    let db = p.db.trim();
    match p.kind {
        Kind::Net => format!("{}//{hp}/{db}{}", p.prefix, p.rest),
        Kind::SqlServer => {
            let dbp = if db.is_empty() { String::new() } else { format!(";databaseName={db}") };
            format!("{}//{hp}{dbp}{}", p.prefix, p.rest)
        }
        Kind::OracleService => format!("{}//{hp}/{db}{}", p.prefix, p.rest),
        Kind::OracleSid => format!("{}{hp}:{db}{}", p.prefix, p.rest),
        Kind::File => format!("{}{db}{}", p.prefix, p.rest),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ida_y_vuelta() {
        for url in [
            "jdbc:postgresql://localhost:5432/ventas",
            "jdbc:postgresql://db.example.com/ventas?sslmode=require&connectTimeout=5",
            "jdbc:postgresql://${DB_HOST}:${DB_PUERTO}/ventas",
            "jdbc:postgresql://${DB_HOST:-localhost}:5432/ventas",
            "jdbc:postgresql://[::1]:5432/ventas",
            "jdbc:mysql://localhost:3306/app?useSSL=false",
            "jdbc:mariadb://localhost/app",
            "jdbc:db2://host:50000/SAMPLE:retrieveMessagesFromServerOnGetMessage=true;",
            "jdbc:firebirdsql://host:3050/C:/datos/base.fdb",
            "jdbc:h2:tcp://localhost:9092/~/prueba;MODE=PostgreSQL",
            "jdbc:sqlserver://srv\\SQLEXPRESS:1433;databaseName=app;encrypt=true;trustServerCertificate=true",
            "jdbc:oracle:thin:@//host:1521/ORCLPDB1",
            "jdbc:oracle:thin:@host:1521:ORCL",
            "jdbc:sqlite:/ruta/archivo.db",
            "jdbc:h2:mem:prueba",
            "jdbc:h2:/datos/base;AUTO_SERVER=TRUE",
        ] {
            let p = parse(url).unwrap_or_else(|| panic!("no entiende {url}"));
            assert_eq!(build(&p), url, "{p:?}");
        }
    }

    #[test]
    fn campos() {
        let p = parse("jdbc:postgresql://${DB_HOST}:${DB_PUERTO}/ventas?ssl=true").unwrap();
        assert_eq!((p.host.as_str(), p.port.as_str(), p.db.as_str(), p.rest.as_str()), ("${DB_HOST}", "${DB_PUERTO}", "ventas", "?ssl=true"));
        let p = parse("jdbc:sqlserver://h;encrypt=true;databaseName=app").unwrap();
        assert_eq!((p.db.as_str(), p.rest.as_str(), default_port(&p)), ("app", ";encrypt=true", "1433"));
        let mut p = parse("jdbc:mysql://localhost/app").unwrap();
        p.port = "3307".into();
        assert_eq!(build(&p), "jdbc:mysql://localhost:3307/app");
        let p = parse("jdbc:oracle:thin:@h:1521:ORCL").unwrap();
        assert_eq!((p.kind, p.db.as_str()), (Kind::OracleSid, "ORCL"));
        // Lo que no se entiende se edita en la URL.
        assert!(parse("jdbc:oracle:thin:@(DESCRIPTION=(ADDRESS=(HOST=h)(PORT=1521)))").is_none());
        assert!(parse("jdbc:postgresql://h1:5432,h2:5432/db").is_none());
        assert!(parse("jdbc:mysql:loadbalance://h1,h2/db").is_none());
        assert!(parse("jdbc:otra:cosa").is_none());
    }
}

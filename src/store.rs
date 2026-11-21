//! Conexiones guardadas: un archivo `clave=valor` por conexión
//! (`<catálogo>/<id>.conexion`). El catálogo es una carpeta normal: se puede
//! poner en una carpeta compartida, un repositorio git o una unidad de red y
//! la usan a la vez varios equipos y programas (formato de texto simple).
//!
//! Contraseñas: o solo en este equipo (`secretos.txt` en la configuración
//! local, permisos 600) o compartidas en el catálogo CIFRADAS con la frase
//! común (`clave_cifrada=`, ver `crypto`). Nunca en claro en el catálogo.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub const EXT: &str = "conexion";

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Conn {
    /// Nombre del archivo sin extensión (estable aunque cambie `name`).
    pub id: String,
    pub name: String,
    pub group: String,
    pub url: String,
    pub user: String,
    /// Clase del driver; vacía = buscarla en los jar por la URL.
    pub driver: String,
    /// Jar o carpetas con jar (relativos al catálogo); vacío = `<catálogo>/drivers`.
    pub jars: Vec<String>,
    /// Propiedades JDBC extra (`prop.clave=valor`).
    pub props: Vec<(String, String)>,
    /// Segundos de espera al conectar.
    pub timeout: u32,
    pub notes: String,
    /// Contraseña compartida cifrada con la frase común (`x1:…`), o vacía.
    pub secret: String,
}

impl Conn {
    pub fn new() -> Conn {
        Conn { timeout: 15, ..Default::default() }
    }

    pub fn to_text(&self) -> String {
        let mut s = String::from("# conector: conexión JDBC (la contraseña, si va, está cifrada)\n");
        let mut kv = |k: &str, v: &str| {
            if !v.is_empty() {
                s.push_str(k);
                s.push('=');
                s.push_str(&esc(v));
                s.push('\n');
            }
        };
        kv("nombre", &self.name);
        kv("grupo", &self.group);
        kv("url", &self.url);
        kv("usuario", &self.user);
        kv("driver", &self.driver);
        for j in &self.jars {
            kv("jar", j);
        }
        kv("timeout", &self.timeout.to_string());
        kv("notas", &self.notes);
        kv("clave_cifrada", &self.secret);
        for (k, v) in &self.props {
            s.push_str("prop.");
            s.push_str(&esc(k).replace('=', "\\="));
            s.push('=');
            s.push_str(&esc(v));
            s.push('\n');
        }
        s
    }

    pub fn from_text(id: &str, text: &str) -> Conn {
        let mut c = Conn { id: id.into(), ..Conn::new() };
        for line in text.lines() {
            let line = line.trim_start();
            if line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = split_kv(line) else { continue };
            let v = unesc(v);
            match k.trim() {
                "nombre" => c.name = v,
                "grupo" => c.group = v,
                "url" => c.url = v,
                "usuario" => c.user = v,
                "driver" => c.driver = v,
                "jar" => c.jars.push(v),
                "timeout" => c.timeout = v.trim().parse().unwrap_or(15),
                "notas" => c.notes = v,
                "clave_cifrada" => c.secret = v,
                k => {
                    if let Some(p) = k.strip_prefix("prop.") {
                        c.props.push((unesc(p), v));
                    }
                }
            }
        }
        if c.name.is_empty() {
            c.name = id.into();
        }
        c
    }

    /// Jar a cargar, ya resueltos (las carpetas se expanden a sus `*.jar`).
    pub fn jar_paths(&self, catalog: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let items: Vec<PathBuf> = if self.jars.is_empty() {
            vec![catalog.join("drivers")]
        } else {
            self.jars.iter().map(|j| resolve(catalog, j)).collect()
        };
        for p in items {
            if p.is_dir() {
                let mut found: Vec<PathBuf> = std::fs::read_dir(&p)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("jar")))
                    .collect();
                found.sort();
                out.extend(found);
            } else if !self.jars.is_empty() {
                out.push(p); // si no existe, el auxiliar lo dirá
            }
        }
        out
    }
}

// ---------- variables por equipo ----------

/// Variables de este equipo (`<config>/variables.txt`, `NOMBRE=valor`).
pub fn load_vars() -> BTreeMap<String, String> {
    parse_vars(&vars_text())
}

pub fn parse_vars(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .filter(|(k, _)| !k.is_empty())
        .collect()
}

pub fn save_vars(text: &str) -> std::io::Result<()> {
    let dir = config_dir().ok_or_else(|| std::io::Error::other("sin carpeta de configuración"))?;
    std::fs::create_dir_all(&dir)?;
    write_atomic(&dir.join("variables.txt"), text.as_bytes(), true)
}

pub fn vars_text() -> String {
    config_dir().and_then(|d| std::fs::read_to_string(d.join("variables.txt")).ok()).unwrap_or_default()
}

/// Sustituye `${NOMBRE}` (variable de este equipo), `${env:NOMBRE}` (entorno)
/// y `${NOMBRE:-por defecto}`. `$${` deja un `${` literal. Error si falta alguna.
pub fn expand(s: &str, vars: &BTreeMap<String, String>) -> Result<String, String> {
    if !s.contains("${") {
        return Ok(s.to_owned());
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("${") {
        if rest[..i].ends_with('$') {
            out.push_str(&rest[..i - 1]);
            out.push_str("${");
            rest = &rest[i + 2..];
            continue;
        }
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        let Some(end) = after.find('}') else { return Err(format!("falta «}}» en: {s}")) };
        let inner = &after[..end];
        let (name, default) = match inner.split_once(":-") {
            Some((n, d)) => (n.trim(), Some(d)),
            None => (inner.trim(), None),
        };
        let value = match name.strip_prefix("env:") {
            Some(e) => std::env::var(e).ok(),
            None => vars.get(name).cloned(),
        };
        match value.or(default.map(str::to_owned)) {
            Some(v) => out.push_str(&v),
            None => return Err(format!("variable sin definir en este equipo: {name}")),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// `~/…` y rutas relativas al catálogo.
pub fn resolve(catalog: &Path, p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(h) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
            return PathBuf::from(h).join(rest);
        }
    }
    let path = Path::new(p);
    if path.is_absolute() { path.into() } else { catalog.join(path) }
}

/// Separa en la primera «=» que no vaya escapada (`\\=`).
fn split_kv(line: &str) -> Option<(&str, &str)> {
    let b = line.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'=' => return Some((&line[..i], &line[i + 1..])),
            _ => i += 1,
        }
    }
    None
}

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            c => o.push(c),
        }
    }
    o
}

fn unesc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.next() {
                Some('n') => o.push('\n'),
                Some('r') => o.push('\r'),
                Some('t') => o.push('\t'),
                Some(c @ ('\\' | '=')) => o.push(c),
                // Escape desconocido (p. ej. `\;` de H2 escrito a mano): se deja tal cual.
                Some(c) => {
                    o.push('\\');
                    o.push(c);
                }
                None => o.push('\\'),
            }
        } else {
            o.push(c);
        }
    }
    o
}

/// Identificador de archivo a partir del nombre: `Producción Oracle` → `produccion-oracle`.
pub fn slug(name: &str) -> String {
    let mut s = String::new();
    for ch in name.chars() {
        let c = match ch {
            'á' | 'à' | 'ä' | 'â' | 'Á' | 'À' | 'Ä' | 'Â' => 'a',
            'é' | 'è' | 'ë' | 'ê' | 'É' | 'È' | 'Ë' | 'Ê' => 'e',
            'í' | 'ì' | 'ï' | 'î' | 'Í' | 'Ì' | 'Ï' | 'Î' => 'i',
            'ó' | 'ò' | 'ö' | 'ô' | 'Ó' | 'Ò' | 'Ö' | 'Ô' => 'o',
            'ú' | 'ù' | 'ü' | 'û' | 'Ú' | 'Ù' | 'Ü' | 'Û' => 'u',
            'ñ' | 'Ñ' => 'n',
            'ç' | 'Ç' => 'c',
            c if c.is_ascii_alphanumeric() => c.to_ascii_lowercase(),
            _ => '-',
        };
        if c == '-' && (s.is_empty() || s.ends_with('-')) {
            continue;
        }
        s.push(c);
    }
    while s.ends_with('-') {
        s.pop();
    }
    if s.is_empty() { "conexion".into() } else { s }
}

// ---------- catálogo ----------

#[derive(Clone, Debug)]
pub struct Entry {
    pub conn: Conn,
    /// Fecha del archivo al leerlo: si cambia al guardar, otro lo modificó.
    pub mtime: Option<SystemTime>,
}

pub struct Catalog {
    pub dir: PathBuf,
}

impl Catalog {
    pub fn path_of(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.{EXT}"))
    }

    pub fn load(&self) -> Vec<Entry> {
        let mut out: Vec<Entry> = std::fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                if p.extension()? != EXT {
                    return None;
                }
                let id = p.file_stem()?.to_str()?.to_owned();
                if id.starts_with('.') {
                    return None;
                }
                let text = std::fs::read_to_string(&p).ok()?;
                Some(Entry { conn: Conn::from_text(&id, &text), mtime: e.metadata().ok()?.modified().ok() })
            })
            .collect();
        out.sort_by(|a, b| {
            (a.conn.group.to_lowercase(), a.conn.name.to_lowercase()).cmp(&(b.conn.group.to_lowercase(), b.conn.name.to_lowercase()))
        });
        out
    }

    /// Firma barata (nombres, tamaños y fechas) para notar cambios de otros equipos.
    pub fn signature(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut items: Vec<(std::ffi::OsString, u64, Option<SystemTime>)> = std::fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == EXT))
            .map(|e| {
                let m = e.metadata().ok();
                (e.file_name(), m.as_ref().map_or(0, |m| m.len()), m.and_then(|m| m.modified().ok()))
            })
            .collect();
        items.sort();
        let mut h = std::collections::hash_map::DefaultHasher::new();
        items.hash(&mut h);
        h.finish()
    }

    /// Id libre para un nombre nuevo.
    pub fn free_id(&self, name: &str) -> String {
        let base = slug(name);
        let mut id = base.clone();
        let mut n = 2;
        while self.path_of(&id).exists() {
            id = format!("{base}-{n}");
            n += 1;
        }
        id
    }

    /// Guarda; con `expected` falla si el archivo cambió desde que se leyó.
    pub fn save(&self, c: &Conn, expected: Option<Option<SystemTime>>) -> Result<Option<SystemTime>, SaveError> {
        std::fs::create_dir_all(&self.dir).map_err(SaveError::Io)?;
        let path = self.path_of(&c.id);
        if let Some(exp) = expected {
            let now = std::fs::metadata(&path).ok().and_then(|m| m.modified().ok());
            if now != exp {
                return Err(SaveError::Changed);
            }
        }
        write_atomic(&path, c.to_text().as_bytes(), false).map_err(SaveError::Io)?;
        Ok(std::fs::metadata(&path).ok().and_then(|m| m.modified().ok()))
    }

    pub fn delete(&self, id: &str) -> std::io::Result<()> {
        std::fs::remove_file(self.path_of(id))
    }
}

#[derive(Debug)]
pub enum SaveError {
    /// Otro equipo o programa lo modificó después de leerlo.
    Changed,
    Io(std::io::Error),
}

/// Escribe a un temporal en la misma carpeta y lo renombra: quien lea a la
/// vez (otro equipo) ve el archivo viejo o el nuevo, nunca uno a medias.
pub fn write_atomic(path: &Path, data: &[u8], private: bool) -> std::io::Result<()> {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("archivo");
    let tmp = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    let res = (|| {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        if private {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let _ = private;
        let mut f = opts.open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

// ---------- configuración local ----------

/// `~/Library/Application Support/conector` (macOS), `%APPDATA%\conector`
/// (Windows) o `~/.config/conector`. `CONECTOR_CONFIG` la cambia (pruebas).
pub fn config_dir() -> Option<PathBuf> {
    let env = |k| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    if let Some(d) = env("CONECTOR_CONFIG") {
        return Some(d);
    }
    let base = if cfg!(target_os = "macos") {
        env("HOME").map(|h| h.join("Library/Application Support"))
    } else if cfg!(windows) {
        env("APPDATA")
    } else {
        env("XDG_CONFIG_HOME").or_else(|| env("HOME").map(|h| h.join(".config")))
    }?;
    Some(base.join("conector"))
}

/// Ajustes locales `clave=valor` en `ajustes.txt`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Settings {
    /// Carpeta del catálogo (compartible); vacía = `<config>/conexiones`.
    pub catalog: String,
    /// Ejecutable de Java; vacío = `JAVA_HOME` o el del PATH.
    pub java: String,
    /// Filas máximas por consulta.
    pub max_rows: u32,
    /// Opciones de la JVM; vacío = `-Xmx384m`.
    pub jvm: String,
    pub dark: Option<bool>,
    /// Servicio de red para otros programas (ver `server`).
    pub serve: bool,
    /// Dirección:puerto donde escucha.
    pub serve_addr: String,
    /// Token que deben mandar los clientes (`Authorization: Bearer …`).
    pub serve_token: String,
    /// Entregar contraseñas por la red (`?clave=1`). Desactivado por defecto.
    pub serve_passwords: bool,
    /// HTTPS (por defecto); `false` = HTTP en claro.
    pub serve_tls: bool,
    /// Certificado y clave propios (PEM); vacíos = autofirmado (ver `tls`).
    pub serve_cert: String,
    pub serve_key: String,
    /// Idioma de la interfaz («es», «en», «fr»); `None` = el del sistema.
    pub lang: Option<String>,
}

pub const DEFAULT_SERVE_ADDR: &str = "0.0.0.0:7470";

/// Token aleatorio de 32 caracteres hexadecimales.
pub fn new_token() -> String {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("sin aleatoriedad del sistema");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

impl Settings {
    pub fn load() -> Settings {
        let mut s = Settings { max_rows: 1000, serve_addr: DEFAULT_SERVE_ADDR.into(), serve_tls: true, ..Default::default() };
        let text = config_dir().and_then(|d| std::fs::read_to_string(d.join("ajustes.txt")).ok()).unwrap_or_default();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let v = v.trim().to_owned();
            match k.trim() {
                "catalogo" => s.catalog = v,
                "java" => s.java = v,
                "jvm" => s.jvm = v,
                "filas" => s.max_rows = v.parse().unwrap_or(1000),
                "tema" => s.dark = Some(v == "oscuro"),
                "servir" => s.serve = v == "si",
                "servir_en" if !v.is_empty() => s.serve_addr = v,
                "servir_token" => s.serve_token = v,
                "servir_claves" => s.serve_passwords = v == "si",
                "servir_tls" => s.serve_tls = v != "no",
                "servir_cert" => s.serve_cert = v,
                "servir_clave_tls" => s.serve_key = v,
                "idioma" if !v.is_empty() => s.lang = Some(v),
                _ => {}
            }
        }
        if let Some(c) = std::env::var_os("CONECTOR_CATALOGO") {
            s.catalog = c.to_string_lossy().into_owned();
        }
        s
    }

    pub fn save(&self) -> std::io::Result<()> {
        let dir = config_dir().ok_or_else(|| std::io::Error::other("sin carpeta de configuración"))?;
        std::fs::create_dir_all(&dir)?;
        let yn = |b: bool| if b { "si" } else { "no" };
        let mut t = format!(
            "catalogo={}\njava={}\njvm={}\nfilas={}\nservir={}\nservir_en={}\nservir_token={}\nservir_claves={}\nservir_tls={}\nservir_cert={}\nservir_clave_tls={}\n",
            self.catalog,
            self.java,
            self.jvm,
            self.max_rows,
            yn(self.serve),
            self.serve_addr,
            self.serve_token,
            yn(self.serve_passwords),
            yn(self.serve_tls),
            self.serve_cert,
            self.serve_key
        );
        if let Some(d) = self.dark {
            t.push_str(if d { "tema=oscuro\n" } else { "tema=claro\n" });
        }
        if let Some(l) = &self.lang {
            t.push_str(&format!("idioma={l}\n"));
        }
        // Lleva el token del servicio: solo para este usuario.
        write_atomic(&dir.join("ajustes.txt"), t.as_bytes(), true)
    }

    pub fn catalog_dir(&self) -> PathBuf {
        if self.catalog.trim().is_empty() {
            config_dir().unwrap_or_else(|| PathBuf::from(".")).join("conexiones")
        } else {
            resolve(Path::new("."), self.catalog.trim())
        }
    }
}

/// Contraseñas recordadas en este equipo (`secretos.txt`, permisos 600),
/// por carpeta de catálogo + id, para que dos catálogos no se mezclen.
#[derive(Default)]
pub struct Secrets {
    map: BTreeMap<String, String>,
}

impl Secrets {
    fn path() -> Option<PathBuf> {
        Some(config_dir()?.join("secretos.txt"))
    }

    pub fn key(catalog: &Path, id: &str) -> String {
        format!("{}|{id}", catalog.display())
    }

    pub fn load() -> Secrets {
        let mut map = BTreeMap::new();
        if let Some(text) = Self::path().and_then(|p| std::fs::read_to_string(p).ok()) {
            for line in text.lines() {
                if let Some((k, v)) = line.split_once('\t') {
                    map.insert(unesc(k), unesc(v));
                }
            }
        }
        Secrets { map }
    }

    /// Clave de la frase común de un catálogo, recordada en este equipo.
    pub fn catalog_key(&self, catalog: &Path) -> Option<crate::crypto::Key> {
        crate::crypto::key_from_hex(self.get(&format!("frase|{}", catalog.display()))?)
    }

    pub fn set_catalog_key(&mut self, catalog: &Path, key: Option<&crate::crypto::Key>) -> std::io::Result<()> {
        self.set(format!("frase|{}", catalog.display()), key.map(crate::crypto::key_to_hex))
    }

    /// Contraseña de una conexión: la de este equipo y, si no, la compartida descifrada.
    pub fn password_for(&self, c: &Conn, catalog: &Path) -> Option<String> {
        if let Some(p) = self.get(&Self::key(catalog, &c.id)) {
            return Some(p.to_owned());
        }
        if c.secret.is_empty() {
            return None;
        }
        crate::crypto::decrypt(&self.catalog_key(catalog)?, &c.secret)
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.map.get(key).map(|s| s.as_str())
    }

    pub fn set(&mut self, key: String, pass: Option<String>) -> std::io::Result<()> {
        let changed = match pass {
            Some(p) => self.map.insert(key, p.clone()).as_deref() != Some(&p),
            None => self.map.remove(&key).is_some(),
        };
        if !changed {
            return Ok(());
        }
        let path = Self::path().ok_or_else(|| std::io::Error::other("sin carpeta de configuración"))?;
        std::fs::create_dir_all(path.parent().unwrap())?;
        let mut t = String::new();
        for (k, v) in &self.map {
            t.push_str(&esc(k).replace('\t', " "));
            t.push('\t');
            t.push_str(&esc(v).replace('\t', "\\t"));
            t.push('\n');
        }
        write_atomic(&path, t.as_bytes(), true)
    }
}

#[cfg(test)]
pub(crate) fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("conector-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_roundtrip() {
        let c = Conn {
            id: "x".into(),
            name: "Producción = Oracle".into(),
            group: "Prod".into(),
            url: "jdbc:oracle:thin:@//h:1521/S?a=b".into(),
            user: "app".into(),
            driver: "oracle.jdbc.OracleDriver".into(),
            jars: vec!["drivers/ojdbc11.jar".into(), "~/libs".into()],
            props: vec![("oracle.net.ssl=x".into(), "a\nb".into())],
            timeout: 7,
            notes: "línea 1\nlínea 2\\".into(),
            secret: "x1:abc=".into(),
        };
        assert_eq!(Conn::from_text("x", &c.to_text()), c);
    }

    #[test]
    fn hand_written_backslashes_survive() {
        let c = Conn::from_text("h", "url=jdbc:h2:mem:x;INIT=A\\;B\\\\C\\nD\n");
        assert_eq!(c.url, "jdbc:h2:mem:x;INIT=A\\;B\\C\nD");
        assert_eq!(Conn::from_text("h", &c.to_text()).url, c.url);
    }

    #[test]
    fn variables() {
        let v = parse_vars("# comentario\nHOST = db.local\nPUERTO=5433\n");
        assert_eq!(expand("jdbc:postgresql://${HOST}:${PUERTO}/app", &v).unwrap(), "jdbc:postgresql://db.local:5433/app");
        assert_eq!(expand("${NADA:-localhost}:${PUERTO:-1}", &v).unwrap(), "localhost:5433");
        assert_eq!(expand("$${HOST}", &v).unwrap(), "${HOST}");
        assert!(expand("${NADA}", &v).unwrap_err().contains("NADA"));
        assert!(expand("${HOST", &v).is_err());
        // SAFETY: variable propia de este test.
        unsafe { std::env::set_var("CONECTOR_PRUEBA_VAR", "x1") };
        assert_eq!(expand("${env:CONECTOR_PRUEBA_VAR}", &v).unwrap(), "x1");
        assert_eq!(expand("sin variables", &v).unwrap(), "sin variables");
    }

    #[test]
    fn slugs() {
        assert_eq!(slug("Producción Oracle"), "produccion-oracle");
        assert_eq!(slug("  ¡¡ "), "conexion");
        assert_eq!(slug("a__B..c"), "a-b-c");
    }

    #[test]
    fn save_detects_external_change() {
        let dir = temp_dir("cat");
        let cat = Catalog { dir: dir.clone() };
        let mut c = Conn::new();
        c.name = "Uno".into();
        c.id = cat.free_id(&c.name);
        let m = cat.save(&c, Some(None)).unwrap();
        assert_eq!(cat.free_id("Uno"), "uno-2");
        // Otro equipo lo modifica.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(cat.path_of("uno"), "nombre=Otro\n").unwrap();
        assert!(matches!(cat.save(&c, Some(m)), Err(SaveError::Changed)));
        let list = cat.load();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].conn.name, "Otro");
        // Forzado (sin comprobar).
        cat.save(&c, None).unwrap();
        assert_eq!(cat.load()[0].conn.name, "Uno");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn jars_from_drivers_folder() {
        let dir = temp_dir("jars");
        std::fs::create_dir_all(dir.join("drivers")).unwrap();
        std::fs::write(dir.join("drivers/b.jar"), "").unwrap();
        std::fs::write(dir.join("drivers/a.JAR"), "").unwrap();
        std::fs::write(dir.join("drivers/leeme.txt"), "").unwrap();
        let c = Conn::new();
        assert_eq!(c.jar_paths(&dir), vec![dir.join("drivers/a.JAR"), dir.join("drivers/b.jar")]);
        // Ruta absoluta (en cualquier sistema) y relativa al catálogo.
        let abs = std::env::temp_dir().join("x.jar");
        let c = Conn { jars: vec![abs.display().to_string(), "libs/y.jar".into()], ..Conn::new() };
        assert_eq!(c.jar_paths(&dir), vec![abs, dir.join("libs/y.jar")]);
        let _ = std::fs::remove_dir_all(dir);
    }
}

//! Descargas: Java portátil (JRE de Eclipse Temurin, API de Adoptium) en
//! `<config>/java/` y drivers JDBC desde Maven Central en `<catálogo>/drivers/`.
//! Usa `curl` y `tar` del sistema (Windows 10+ los trae) como buscador: nada
//! de TLS en el binario.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

/// Estado de una descarga en curso, compartido con la interfaz.
#[derive(Clone, Debug, PartialEq)]
pub enum State {
    Running(String),
    Done(String),
    Failed(String),
}

pub type Shared = Arc<Mutex<State>>;

fn run(cmd: &mut Command) -> Result<(), String> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let out = cmd.output().map_err(|e| format!("{:?}: {e}", cmd.get_program()))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_owned())
    }
}

/// Descarga `url` a `dest` (vía temporal: nunca deja un archivo a medias).
pub fn fetch(url: &str, dest: &Path) -> Result<(), String> {
    let tmp = dest.with_extension("descargando");
    let r = run(Command::new("curl").args(["-fsSL", "--retry", "2", "-o"]).arg(&tmp).arg(url));
    match r.and_then(|_| std::fs::rename(&tmp, dest).map_err(|e| e.to_string())) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(format!("no se pudo descargar {url}: {e}"))
        }
    }
}

fn fetch_text(url: &str) -> Result<String, String> {
    let out = Command::new("curl").args(["-fsSL", "--retry", "2", url]).output().map_err(|e| format!("curl: {e}"))?;
    if !out.status.success() {
        return Err(format!("no se pudo leer {url}: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn spawn(state: &Shared, wake: Arc<dyn Fn() + Send + Sync>, job: impl FnOnce(&dyn Fn(String)) -> Result<String, String> + Send + 'static) {
    let st = state.clone();
    std::thread::spawn(move || {
        let progress = |m: String| {
            *st.lock().unwrap() = State::Running(m);
            wake();
        };
        let r = job(&progress);
        *st.lock().unwrap() = match r {
            Ok(m) => State::Done(m),
            Err(e) => State::Failed(e),
        };
        wake();
    });
}

// ---------- Java portátil ----------

fn java_dir() -> Option<PathBuf> {
    Some(crate::store::config_dir()?.join("java"))
}

/// `bin/java` del Java portátil, si está descargado.
pub fn installed() -> Option<PathBuf> {
    let exe = if cfg!(windows) { "java.exe" } else { "java" };
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(java_dir()?).ok()?.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    dirs.sort();
    dirs.into_iter().rev().find_map(|d| {
        [d.join("bin").join(exe), d.join("Contents/Home/bin").join(exe)].into_iter().find(|p| p.is_file())
    })
}

fn adoptium_url() -> Result<String, String> {
    let os = match std::env::consts::OS {
        "linux" => "linux",
        "macos" => "mac",
        "windows" => "windows",
        o => return Err(format!("sistema sin Java portátil: {o}")),
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "aarch64",
        a => return Err(format!("arquitectura sin Java portátil: {a}")),
    };
    Ok(format!("https://api.adoptium.net/v3/binary/latest/21/ga/{os}/{arch}/jre/hotspot/normal/eclipse"))
}

/// Descarga y descomprime un JRE 21 (unos 50 MB) en `<config>/java/`.
pub fn download_java(state: &Shared, wake: Arc<dyn Fn() + Send + Sync>) {
    *state.lock().unwrap() = State::Running("Preparando…".into());
    spawn(state, wake, |progress| {
        let dir = java_dir().ok_or("sin carpeta de configuración")?;
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let url = adoptium_url()?;
        let archive = dir.join(if cfg!(windows) { "jre.zip" } else { "jre.tar.gz" });
        progress("Descargando Java (Temurin 21 JRE, ~50 MB)…".into());
        fetch(&url, &archive)?;
        progress("Descomprimiendo…".into());
        let r = run(Command::new("tar").arg("-xf").arg(&archive).arg("-C").arg(&dir));
        let _ = std::fs::remove_file(&archive);
        r.map_err(|e| format!("no se pudo descomprimir: {e}"))?;
        let java = installed().ok_or("no se encontró bin/java en lo descargado")?;
        Ok(java.display().to_string())
    });
}

// ---------- drivers desde Maven Central ----------

/// Grupo y artefacto en Maven Central de cada plantilla (nombre de `app::TEMPLATES`).
pub fn maven_coords(template: &str) -> Option<(&'static str, &'static str)> {
    Some(match template {
        "PostgreSQL" => ("org.postgresql", "postgresql"),
        "MySQL" => ("com.mysql", "mysql-connector-j"),
        "MariaDB" => ("org.mariadb.jdbc", "mariadb-java-client"),
        "Oracle" => ("com.oracle.database.jdbc", "ojdbc11"),
        "SQL Server" => ("com.microsoft.sqlserver", "mssql-jdbc"),
        "DB2" => ("com.ibm.db2", "jcc"),
        "Firebird" => ("org.firebirdsql.jdbc", "jaybird"),
        "H2 (memoria)" => ("com.h2database", "h2"),
        "SQLite" => ("org.xerial", "sqlite-jdbc"),
        _ => return None,
    })
}

/// Última versión estable de `maven-metadata.xml` (sin preview/alpha/beta/rc;
/// en SQL Server, la de Java 11: `.jre11`).
pub fn pick_version(metadata: &str, artifact: &str) -> Option<String> {
    let versions: Vec<&str> = metadata
        .split("<version>")
        .skip(1)
        .filter_map(|s| s.split("</version>").next())
        .map(str::trim)
        .collect();
    let unstable = ["preview", "alpha", "beta", "rc", "snapshot", "-m"];
    versions
        .into_iter()
        .filter(|v| !unstable.iter().any(|u| v.to_ascii_lowercase().contains(u)))
        .filter(|v| artifact != "mssql-jdbc" || v.ends_with(".jre11"))
        .max_by(|a, b| version_key(a).cmp(&version_key(b)))
        .map(String::from)
}

fn version_key(v: &str) -> Vec<u64> {
    v.split(|c: char| !c.is_ascii_digit()).filter(|s| !s.is_empty()).map(|s| s.parse().unwrap_or(0)).collect()
}

/// Descarga el jar del driver a `<catálogo>/drivers/`.
pub fn download_driver(state: &Shared, wake: Arc<dyn Fn() + Send + Sync>, template: String, catalog: PathBuf) {
    *state.lock().unwrap() = State::Running(format!("Buscando el driver de {template}…"));
    spawn(state, wake, move |progress| {
        let (g, a) = maven_coords(&template).ok_or("sin driver conocido para esta plantilla")?;
        let base = format!("https://repo1.maven.org/maven2/{}/{a}", g.replace('.', "/"));
        let meta = fetch_text(&format!("{base}/maven-metadata.xml"))?;
        let v = pick_version(&meta, a).ok_or("no hay versión estable publicada")?;
        let dir = catalog.join("drivers");
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let name = format!("{a}-{v}.jar");
        let dest = dir.join(&name);
        if !dest.exists() {
            progress(format!("Descargando {name}…"));
            fetch(&format!("{base}/{v}/{name}"), &dest)?;
        }
        Ok(format!("drivers/{name}"))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        let m = "<metadata><versioning><versions><version>42.7.4</version><version>42.10.0</version>\
                 <version>43.0.0-rc1</version><version>42.9.1</version></versions></versioning></metadata>";
        assert_eq!(pick_version(m, "postgresql").as_deref(), Some("42.10.0"));
        let ms = "<version>12.8.1.jre11</version><version>12.8.1.jre8</version><version>13.1.0.jre11-preview</version><version>12.10.0.jre11</version>";
        assert_eq!(pick_version(ms, "mssql-jdbc").as_deref(), Some("12.10.0.jre11"));
    }
}

//! Túneles SSH para llegar a bases que solo se ven desde un bastión.
//!
//! Usa el `ssh` del sistema (OpenSSH: Linux, macOS y Windows 10+), así valen
//! las claves, el agente, `~/.ssh/config` y ProxyJump de cada equipo, sin
//! meter un cliente SSH en el binario. Siempre `BatchMode=yes`: nunca pide
//! contraseña (si la clave no vale, falla con el mensaje de ssh).
//!
//! `ssh -N -L 127.0.0.1:<libre>:<destino> usuario@bastión`, y en la URL JDBC el
//! `host:puerto` del destino se cambia por `127.0.0.1:<libre>`. Un túnel por
//! (bastión, destino, clave) que se reutiliza mientras siga vivo; se cierran
//! al salir (y en Linux, si conector muere, ssh recibe SIGTERM).

use std::collections::HashMap;
use std::io::Read;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Spec {
    /// `usuario@bastión[:puerto]`.
    pub jump: String,
    /// Clave privada (opcional; si no, la del agente o ~/.ssh).
    pub key: String,
    /// Destino visto desde el bastión, `host:puerto` (vacío = el de la URL).
    pub dest: String,
    /// Opciones extra de ssh (p. ej. `-o ProxyJump=otro`).
    pub opts: String,
}

struct Tunnel {
    child: Child,
    port: u16,
}

fn tunnels() -> &'static Mutex<HashMap<String, Tunnel>> {
    static T: OnceLock<Mutex<HashMap<String, Tunnel>>> = OnceLock::new();
    T.get_or_init(Default::default)
}

/// Puertos por defecto según el tipo de URL.
fn default_port(url: &str) -> Option<u16> {
    let u = url.to_ascii_lowercase();
    Some(match () {
        _ if u.starts_with("jdbc:postgresql:") => 5432,
        _ if u.starts_with("jdbc:mysql:") || u.starts_with("jdbc:mariadb:") => 3306,
        _ if u.starts_with("jdbc:oracle:") => 1521,
        _ if u.starts_with("jdbc:sqlserver:") => 1433,
        _ if u.starts_with("jdbc:db2:") => 50000,
        _ if u.starts_with("jdbc:firebirdsql:") => 3050,
        _ if u.starts_with("jdbc:h2:tcp:") => 9092,
        _ => return None,
    })
}

/// Dónde está `host[:puerto]` en la URL: (inicio, fin, host, puerto).
/// Formatos: `//host:puerto/…` (casi todos), `@//host:puerto/…` y
/// `@host:puerto:sid` (Oracle), `//host:puerto;…` (SQL Server).
pub fn find_host(url: &str) -> Option<(usize, usize, String, Option<u16>)> {
    let start = if let Some(i) = url.find("@//") {
        i + 3
    } else if let Some(i) = url.find("://") {
        i + 3
    } else {
        url.find('@')? + 1
    };
    let rest = &url[start..];
    // IPv6 entre corchetes.
    let (host, mut end) = if rest.starts_with('[') {
        let close = rest.find(']')?;
        (rest[..=close].to_owned(), start + close + 1)
    } else {
        let n = rest.find([':', '/', ';', '?', ',']).unwrap_or(rest.len());
        (rest[..n].to_owned(), start + n)
    };
    if host.is_empty() {
        return None;
    }
    let mut port = None;
    if url[end..].starts_with(':') {
        let digits: String = url[end + 1..].chars().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() {
            port = digits.parse().ok();
            end += 1 + digits.len();
        }
    }
    Some((start, end, host, port))
}

/// URL con el destino cambiado por el túnel local, y el destino `host:puerto`.
pub fn rewrite(url: &str, dest: &str, local: u16) -> Result<(String, String), String> {
    let (s, e, host, port) = find_host(url).ok_or("no se encuentra el servidor en la URL para el túnel SSH")?;
    let target = if dest.trim().is_empty() {
        let p = port.or_else(|| default_port(url)).ok_or("indica el destino del túnel (host:puerto)")?;
        format!("{host}:{p}")
    } else {
        dest.trim().to_owned()
    };
    Ok((format!("{}127.0.0.1:{local}{}", &url[..s], &url[e..]), target))
}

fn free_port() -> Result<u16, String> {
    TcpListener::bind("127.0.0.1:0").and_then(|l| l.local_addr()).map(|a| a.port()).map_err(|e| e.to_string())
}

/// Puerto local de un túnel abierto (lo crea si hace falta). Bloquea hasta
/// que el túnel acepta conexiones o falla (`timeout`).
pub fn ensure(spec: &Spec, target: &str, timeout: Duration) -> Result<u16, String> {
    let key = format!("{}|{}|{}|{}", spec.jump, target, spec.key, spec.opts);
    let mut map = tunnels().lock().unwrap();
    if let Some(t) = map.get_mut(&key) {
        if matches!(t.child.try_wait(), Ok(None)) {
            return Ok(t.port);
        }
        map.remove(&key);
    }
    let (user_host, ssh_port) = match spec.jump.trim().rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !h.ends_with(']') => (h.to_owned(), Some(p.to_owned())),
        _ => (spec.jump.trim().to_owned(), None),
    };
    if user_host.is_empty() {
        return Err("falta el bastión SSH (usuario@servidor)".into());
    }
    let port = free_port()?;
    let ssh = std::env::var_os("CONECTOR_SSH").unwrap_or_else(|| "ssh".into());
    let mut cmd = Command::new(ssh);
    cmd.args(["-N", "-o", "BatchMode=yes", "-o", "ExitOnForwardFailure=yes", "-o", "ServerAliveInterval=30", "-o", "StrictHostKeyChecking=accept-new"]);
    cmd.arg("-o").arg(format!("ConnectTimeout={}", timeout.as_secs().clamp(3, 60)));
    cmd.arg("-L").arg(format!("127.0.0.1:{port}:{target}"));
    if let Some(p) = ssh_port {
        cmd.arg("-p").arg(p);
    }
    if !spec.key.trim().is_empty() {
        let k = crate::store::resolve(std::path::Path::new("."), spec.key.trim());
        cmd.arg("-i").arg(k).args(["-o", "IdentitiesOnly=yes"]);
    }
    cmd.args(spec.opts.split_whitespace());
    cmd.arg(&user_host);
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        // Si conector muere sin cerrar el túnel, el kernel avisa a ssh.
        // SAFETY: prctl solo cambia un atributo del proceso hijo antes de exec.
        unsafe {
            cmd.pre_exec(|| {
                unsafe extern "C" {
                    fn prctl(option: i32, arg2: u64, ...) -> i32;
                }
                prctl(1 /* PR_SET_PDEATHSIG */, 15 /* SIGTERM */);
                Ok(())
            });
        }
    }
    let mut child = spawn_long_lived(cmd).map_err(|e| format!("no se pudo ejecutar ssh ({e}): instala OpenSSH"))?;
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let start = Instant::now();
    loop {
        if let Ok(Some(st)) = child.try_wait() {
            let mut err = String::new();
            if let Some(mut e) = child.stderr.take() {
                let _ = e.read_to_string(&mut err);
            }
            let err = err.lines().filter(|l| !l.contains("Permanently added")).collect::<Vec<_>>().join(" ");
            return Err(format!("el túnel SSH a {user_host} falló ({st}): {}", if err.is_empty() { "sin detalle" } else { &err }));
        }
        if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
            break;
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("el túnel SSH a {user_host} no respondió en {} s", timeout.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // stderr ya no se lee: que no se llene la tubería.
    if let Some(mut e) = child.stderr.take() {
        std::thread::spawn(move || {
            let mut sink = [0u8; 1024];
            while matches!(e.read(&mut sink), Ok(n) if n > 0) {}
        });
    }
    map.insert(key, Tunnel { child, port });
    Ok(port)
}

/// Lanza el proceso desde un hilo que vive tanto como conector: en Linux,
/// PR_SET_PDEATHSIG avisa cuando muere el HILO que lo creó, no el proceso.
fn spawn_long_lived(cmd: Command) -> std::io::Result<Child> {
    type Job = (Command, std::sync::mpsc::Sender<std::io::Result<Child>>);
    static TX: OnceLock<Mutex<std::sync::mpsc::Sender<Job>>> = OnceLock::new();
    let tx = TX.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("tuneles".into())
            .spawn(move || {
                for (mut cmd, reply) in rx {
                    let _ = reply.send(cmd.spawn());
                }
            })
            .expect("hilo");
        Mutex::new(tx)
    });
    let (rtx, rrx) = std::sync::mpsc::channel();
    tx.lock().unwrap().send((cmd, rtx)).map_err(|_| std::io::Error::other("hilo de túneles parado"))?;
    rrx.recv().map_err(|_| std::io::Error::other("hilo de túneles parado"))?
}

/// Cierra todos los túneles (al salir).
pub fn close_all() {
    if let Some(m) = tunnels().lock().ok().as_mut() {
        for (_, mut t) in m.drain() {
            let _ = t.child.kill();
            let _ = t.child.wait();
        }
    }
}

/// Túneles abiertos (para mostrarlo en la interfaz).
pub fn open_count() -> usize {
    tunnels().lock().map(|mut m| m.values_mut().map(|t| matches!(t.child.try_wait(), Ok(None)) as usize).sum()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_urls() {
        let r = |u: &str| rewrite(u, "", 40000).unwrap();
        assert_eq!(r("jdbc:postgresql://db.interna:5433/app?ssl=true"), ("jdbc:postgresql://127.0.0.1:40000/app?ssl=true".into(), "db.interna:5433".into()));
        assert_eq!(r("jdbc:postgresql://db/app"), ("jdbc:postgresql://127.0.0.1:40000/app".into(), "db:5432".into()));
        assert_eq!(r("jdbc:oracle:thin:@//ora:1521/SVC"), ("jdbc:oracle:thin:@//127.0.0.1:40000/SVC".into(), "ora:1521".into()));
        assert_eq!(r("jdbc:oracle:thin:@ora:1522:ORCL"), ("jdbc:oracle:thin:@127.0.0.1:40000:ORCL".into(), "ora:1522".into()));
        assert_eq!(r("jdbc:sqlserver://sql;databaseName=x"), ("jdbc:sqlserver://127.0.0.1:40000;databaseName=x".into(), "sql:1433".into()));
        assert_eq!(r("jdbc:mysql://[fd00::5]:3307/x"), ("jdbc:mysql://127.0.0.1:40000/x".into(), "[fd00::5]:3307".into()));
        // Destino explícito: se respeta.
        assert_eq!(rewrite("jdbc:h2:tcp://nombre-interno/mem:t", "localhost:9092", 1).unwrap().1, "localhost:9092");
        assert!(rewrite("jdbc:h2:mem:x", "", 1).is_err());
    }
}

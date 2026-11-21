//! Puente con Java: un único proceso auxiliar (`java/ConectorHelper.java`,
//! compilado en `java/auxiliar.jar` e incrustado en el binario: basta un JRE 8+)
//! que se arranca al primer uso y habla el protocolo de `proto`.
//!
//! Por qué un proceso aparte y no JNI: la JVM no se puede descargar ni
//! reiniciar dentro de un proceso, un driver que se cuelga o revienta se
//! llevaría la interfaz, y no hace falta Java para compilar conector. Así la interfaz
//! queda ligera (sin JVM hasta que se usa) y «Reiniciar Java» mata conexiones
//! colgadas.
//!
//! Hilos: uno escribe (y arranca el proceso, sin bloquear a nadie), uno lee
//! stdout y otro guarda las últimas líneas de stderr para los errores.

use crate::proto::{self, Msg};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Generado con `scripts/compilar-auxiliar.sh` (el test `auxiliar_al_dia` lo vigila).
const JAR: &[u8] = include_bytes!("java/auxiliar.jar");
#[cfg(test)]
const SOURCE: &str = include_str!("java/ConectorHelper.java");
const CLASS: &str = "ConectorHelper";
const STDERR_LINES: usize = 40;

#[derive(Debug)]
pub enum Event {
    Msg(Msg),
    /// No se pudo enviar la petición `id` (Java no arranca, se cerró…).
    Failed(u64, String),
    /// El proceso terminó; texto = últimas líneas de stderr.
    Exited(String),
}

enum Cmd {
    Send(u64, String),
    /// La orden `id` no se pudo enviar (p. ej. falló su túnel SSH).
    Fail(u64, String),
    Restart,
    Stop,
}

#[derive(Clone, Default)]
pub struct Config {
    /// Ejecutable de Java; vacío = `JAVA_HOME` o `java` del PATH.
    pub java: String,
    /// Opciones extra de la JVM (separadas por espacios).
    pub jvm_opts: String,
}

pub struct Bridge {
    handle: Handle,
    pub rx: Receiver<Event>,
    running: Arc<AtomicBool>,
}

/// Para enviar órdenes desde varios hilos (el servicio de red).
#[derive(Clone)]
pub struct Handle {
    tx: Sender<Cmd>,
    next: Arc<AtomicU64>,
}

impl Handle {
    /// Envía una orden; devuelve su id (las respuestas llevan el mismo).
    /// Si lleva `ssh=`, primero abre (o reutiliza) el túnel en otro hilo y
    /// cambia la URL por la local; si el túnel falla, llega `Event::Failed`.
    pub fn send(&self, cmd: &str, fields: &[(&str, &str)]) -> u64 {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let get = |k: &str| fields.iter().find(|(a, _)| *a == k).map(|(_, v)| v.to_string()).unwrap_or_default();
        let jump = get("ssh");
        if jump.is_empty() {
            let _ = self.tx.send(Cmd::Send(id, proto::line(cmd, id, fields)));
            return id;
        }
        let spec = crate::tunnel::Spec { jump, key: get("ssh_clave"), dest: get("ssh_destino"), opts: get("ssh_opciones") };
        let timeout = Duration::from_secs(get("timeout").parse::<u64>().unwrap_or(15).clamp(5, 120));
        let mut rest: Vec<(String, String)> = fields.iter().filter(|(k, _)| !k.starts_with("ssh")).map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let (tx, cmd) = (self.tx.clone(), cmd.to_owned());
        std::thread::spawn(move || {
            let url = rest.iter().find(|(k, _)| k == "url").map(|(_, v)| v.clone()).unwrap_or_default();
            let r = crate::tunnel::rewrite(&url, &spec.dest, 0)
                .and_then(|(_, target)| crate::tunnel::ensure(&spec, &target, timeout))
                .and_then(|port| crate::tunnel::rewrite(&url, &spec.dest, port));
            match r {
                Ok((local_url, _)) => {
                    for (k, v) in &mut rest {
                        if k == "url" {
                            *v = local_url.clone();
                        }
                    }
                    let refs: Vec<(&str, &str)> = rest.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
                    let _ = tx.send(Cmd::Send(id, proto::line(&cmd, id, &refs)));
                }
                Err(e) => {
                    let _ = tx.send(Cmd::Fail(id, e));
                }
            }
        });
        id
    }

    /// Mata el proceso (y con él conexiones y consultas colgadas).
    pub fn restart(&self) {
        let _ = self.tx.send(Cmd::Restart);
    }
}

type Waker = Arc<dyn Fn() + Send + Sync>;

impl Bridge {
    /// `wake` se llama al llegar cada evento (p. ej. `ctx.request_repaint`).
    pub fn new(cfg: Config, wake: Waker) -> Bridge {
        let (tx, crx) = mpsc::channel();
        let (etx, rx) = mpsc::channel();
        let running = Arc::new(AtomicBool::new(false));
        let r = running.clone();
        std::thread::Builder::new()
            .name("java-escritor".into())
            .spawn(move || writer(cfg, crx, etx, wake, r))
            .expect("hilo");
        Bridge { handle: Handle { tx, next: Arc::new(AtomicU64::new(1)) }, rx, running }
    }

    pub fn handle(&self) -> Handle {
        self.handle.clone()
    }

    pub fn running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    pub fn send(&mut self, cmd: &str, fields: &[(&str, &str)]) -> u64 {
        self.handle.send(cmd, fields)
    }

    pub fn restart(&self) {
        self.handle.restart();
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        let _ = self.handle.tx.send(Cmd::Stop);
    }
}

struct Proc {
    child: Child,
    stdin: ChildStdin,
}

fn writer(cfg: Config, rx: Receiver<Cmd>, ev: Sender<Event>, wake: Waker, running: Arc<AtomicBool>) {
    let mut proc: Option<Proc> = None;
    let stop = |p: &mut Option<Proc>, polite: bool| {
        if let Some(mut pr) = p.take() {
            if polite {
                let _ = pr.stdin.write_all(b"salir\n");
                let _ = pr.stdin.flush();
                drop(pr.stdin);
                for _ in 0..20 {
                    if matches!(pr.child.try_wait(), Ok(Some(_))) {
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
            }
            let _ = pr.child.kill();
            let _ = pr.child.wait();
        }
    };
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Stop => break,
            Cmd::Restart => stop(&mut proc, false),
            Cmd::Fail(id, e) => {
                let _ = ev.send(Event::Failed(id, e));
                wake();
            }
            Cmd::Send(id, line) => {
                if proc.as_mut().is_some_and(|p| !matches!(p.child.try_wait(), Ok(None))) {
                    proc = None; // murió; se arranca otro
                }
                if proc.is_none() {
                    match start(&cfg, ev.clone(), wake.clone(), running.clone()) {
                        Ok(p) => proc = Some(p),
                        Err(e) => {
                            let _ = ev.send(Event::Failed(id, e));
                            wake();
                            continue;
                        }
                    }
                }
                let p = proc.as_mut().unwrap();
                let ok = p.stdin.write_all(line.as_bytes()).and_then(|_| p.stdin.write_all(b"\n")).and_then(|_| p.stdin.flush());
                if let Err(e) = ok {
                    let _ = ev.send(Event::Failed(id, format!("no se pudo hablar con Java: {e}")));
                    wake();
                    stop(&mut proc, false);
                }
            }
        }
    }
    stop(&mut proc, true);
}

fn start(cfg: &Config, ev: Sender<Event>, wake: Waker, running: Arc<AtomicBool>) -> Result<Proc, String> {
    let mut cmd = command(cfg)?;
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    hide_console(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| format!("no se pudo ejecutar Java ({e}). Configura la ruta de Java en Ajustes o JAVA_HOME."))?;
    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let tail = Arc::new(Mutex::new(VecDeque::<String>::new()));
    let t = tail.clone();
    std::thread::Builder::new()
        .name("java-stderr".into())
        .spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let mut q = t.lock().unwrap();
                if q.len() == STDERR_LINES {
                    q.pop_front();
                }
                q.push_back(line);
            }
        })
        .map_err(|e| e.to_string())?;
    running.store(true, Ordering::Relaxed);
    std::thread::Builder::new()
        .name("java-lector".into())
        .spawn(move || {
            let mut r = BufReader::with_capacity(1 << 16, stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match r.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let l = line.trim_end_matches(['\n', '\r']);
                if let Some(m) = Msg::parse(l) {
                    // Las filas llegan en ráfagas: despertar solo al final de un bloque.
                    let quiet = &*m.kind == "fila" && !r.buffer().is_empty();
                    if ev.send(Event::Msg(m)).is_err() {
                        break;
                    }
                    if !quiet {
                        wake();
                    }
                }
            }
            running.store(false, Ordering::Relaxed);
            // Dar tiempo a que stderr termine de leerse.
            std::thread::sleep(std::time::Duration::from_millis(50));
            let text = tail.lock().unwrap().iter().cloned().collect::<Vec<_>>().join("\n");
            let _ = ev.send(Event::Exited(text));
            wake();
        })
        .map_err(|e| e.to_string())?;
    Ok(Proc { child, stdin })
}

#[cfg(windows)]
fn hide_console(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
}
#[cfg(not(windows))]
fn hide_console(_: &mut Command) {}

/// Campos de `probar`/`abrir` para una conexión, con las variables de este
/// equipo ya sustituidas (`store::expand`) en URL, usuario, jar y propiedades.
pub fn conn_fields(c: &crate::store::Conn, password: &str, catalog: &Path) -> Result<Vec<(String, String)>, String> {
    let vars = crate::store::load_vars();
    let x = |s: &str| crate::store::expand(s, &vars);
    let mut c = c.clone();
    c.url = x(c.url.trim())?;
    c.user = x(&c.user)?;
    c.jars = c.jars.iter().map(|j| x(j)).collect::<Result<_, _>>()?;
    let mut f = vec![("url".to_owned(), c.url.clone()), ("timeout".to_owned(), c.timeout.max(1).to_string())];
    if !c.user.is_empty() {
        f.push(("usuario".into(), c.user.clone()));
    }
    if !password.is_empty() {
        f.push(("clave".into(), password.to_owned()));
    }
    if !c.driver.trim().is_empty() {
        f.push(("driver".into(), c.driver.trim().to_owned()));
    }
    for j in c.jar_paths(catalog) {
        f.push(("jar".into(), j.display().to_string()));
    }
    for (k, v) in &c.props {
        f.push((format!("prop.{k}"), x(v)?));
    }
    // Túnel SSH: lo abre `Handle::send` antes de mandar la orden (ver `tunnel`).
    if !c.ssh.trim().is_empty() {
        f.push(("ssh".into(), x(c.ssh.trim())?));
        f.push(("ssh_clave".into(), x(c.ssh_key.trim())?));
        f.push(("ssh_destino".into(), x(c.ssh_dest.trim())?));
        f.push(("ssh_opciones".into(), x(c.ssh_opts.trim())?));
    }
    Ok(f)
}

/// Ejecutable de Java: ajuste → Java portátil descargado → `JAVA_HOME` → PATH.
pub fn java_path(setting: &str) -> PathBuf {
    let exe = if cfg!(windows) { "java.exe" } else { "java" };
    if !setting.trim().is_empty() {
        let p = PathBuf::from(setting.trim());
        return if p.is_dir() { p.join("bin").join(exe) } else { p };
    }
    if let Some(p) = crate::javadl::installed() {
        return p;
    }
    if let Some(h) = std::env::var_os("JAVA_HOME").filter(|h| !h.is_empty()) {
        let p = PathBuf::from(h).join("bin").join(exe);
        if p.exists() {
            return p;
        }
    }
    PathBuf::from(exe)
}

/// ¿Hay Java que ejecutar? (para ofrecer la descarga del portátil)
pub fn java_available(setting: &str) -> bool {
    let p = java_path(setting);
    if p.components().count() > 1 {
        return p.is_file();
    }
    std::env::var_os("PATH").is_some_and(|path| std::env::split_paths(&path).any(|d| d.join(&p).is_file()))
}

fn command(cfg: &Config) -> Result<Command, String> {
    // Pruebas: un auxiliar falso (`sh -c`).
    if let Some(fake) = std::env::var_os("CONECTOR_AUXILIAR") {
        let mut c = Command::new("sh");
        c.arg("-c").arg(fake);
        return Ok(c);
    }
    let java = java_path(&cfg.java);
    let jar = helper_jar()?;
    let mut c = Command::new(&java);
    // JVM pequeña y de arranque rápido: las filas no se acumulan en Java.
    c.args(["-Xss512k", "-XX:+UseSerialGC", "-XX:TieredStopAtLevel=1", "-Dfile.encoding=UTF-8"]);
    let opts = if cfg.jvm_opts.trim().is_empty() { "-Xmx384m" } else { cfg.jvm_opts.as_str() };
    c.args(opts.split_whitespace());
    c.arg("-cp").arg(jar).arg(CLASS);
    Ok(c)
}

/// Deja el jar incrustado en `<config>/auxiliar/<hash>.jar` (una vez por versión).
fn helper_jar() -> Result<PathBuf, String> {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    JAR.hash(&mut h);
    let dir = crate::store::config_dir().ok_or("sin carpeta de configuración")?.join("auxiliar");
    let path = dir.join(format!("{:016x}.jar", h.finish()));
    if std::fs::metadata(&path).map(|m| m.len()).ok() != Some(JAR.len() as u64) {
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        crate::store::write_atomic(&path, JAR, false).map_err(|e| e.to_string())?;
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Auxiliar falso en sh: contesta `ok` con la orden recibida y `fila` para consultas.
    pub(crate) const FAKE: &str = r#"printf 'listo\t0\tjava=falso\n'
while IFS= read -r l; do
  cmd=$(printf '%s' "$l" | cut -f1); id=$(printf '%s' "$l" | cut -f2)
  case "$cmd" in
    salir) exit 0 ;;
    morir) echo "adiós desde stderr" >&2; exit 3 ;;
    consulta) printf 'columnas\t%s\tA\001INT\tB\001VARCHAR\n' "$id"
              printf 'fila\t%s\t1\t\\N\n' "$id"; printf 'fila\t%s\t2\tdos\\tt\n' "$id"
              printf 'fin\t%s\tfilas=2\tmas=false\tactualizadas=\tms=1\n' "$id" ;;
    *) printf 'ok\t%s\torden=%s\tms=5\tproducto=Falsa\n' "$id" "$cmd" ;;
  esac
done"#;

    /// El jar incrustado debe corresponder al .java (va sin comprimir dentro).
    #[test]
    fn auxiliar_al_dia() {
        let found = JAR.windows(SOURCE.len()).any(|w| w == SOURCE.as_bytes());
        assert!(found, "src/java/auxiliar.jar no corresponde a ConectorHelper.java: ejecuta scripts/compilar-auxiliar.sh");
    }

    fn next(b: &Bridge) -> Event {
        b.rx.recv_timeout(Duration::from_secs(5)).expect("sin respuesta")
    }

    /// El auxiliar falso es un script de sh: solo en Unix.
    #[cfg(unix)]
    #[test]
    fn fake_helper_roundtrip_and_restart() {
        // SAFETY: solo este test usa CONECTOR_AUXILIAR.
        unsafe { std::env::set_var("CONECTOR_AUXILIAR", FAKE) };
        let mut b = Bridge::new(Config::default(), Arc::new(|| {}));
        let id = b.send("probar", &[("url", "jdbc:x")]);
        let Event::Msg(m) = next(&b) else { panic!() };
        assert_eq!(&*m.kind, "listo");
        let Event::Msg(m) = next(&b) else { panic!() };
        assert_eq!((m.id, m.get("orden")), (id, Some("probar")));
        assert!(b.running());

        let q = b.send("consulta", &[("sql", "select 1\nfrom t")]);
        let kinds: Vec<(String, Vec<Option<Box<str>>>)> = (0..4)
            .map(|_| match next(&b) {
                Event::Msg(m) => {
                    assert_eq!(m.id, q);
                    (m.kind.to_string(), m.fields)
                }
                e => panic!("{e:?}"),
            })
            .collect();
        assert_eq!(kinds[0].0, "columnas");
        assert_eq!(kinds[1].1, vec![Some("1".into()), None]);
        assert_eq!(kinds[2].1, vec![Some("2".into()), Some("dos\tt".into())]);
        assert_eq!(kinds[3].0, "fin");

        // El proceso muere: llega Exited con su stderr y la siguiente orden lo rearranca.
        b.send("morir", &[]);
        match next(&b) {
            Event::Exited(t) => assert!(t.contains("adiós"), "{t}"),
            e => panic!("{e:?}"),
        }
        let id = b.send("abrir", &[]);
        let Event::Msg(m) = next(&b) else { panic!() };
        assert_eq!(&*m.kind, "listo");
        let Event::Msg(m) = next(&b) else { panic!() };
        assert_eq!(m.id, id);
        b.restart();
        assert!(matches!(next(&b), Event::Exited(_)));
    }
}

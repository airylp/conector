//! Servicio de red para que otros programas (en otros equipos, en cualquier
//! lenguaje) usen las conexiones del catálogo: HTTP/1.1 + JSON, solo con la
//! biblioteca estándar. Siempre con token (`Authorization: Bearer <token>`).
//!
//!   GET  /v1/estado                          versión, nº de conexiones, firma
//!   GET  /v1/conexiones                      lista (sin contraseñas)
//!   GET  /v1/conexiones/{id}[?clave=1]       una; con clave=1 incluye la contraseña
//!                                            si el servicio lo permite (servir_claves)
//!   POST /v1/conexiones/{id}/probar          prueba la conexión
//!   POST /v1/conexiones/{id}/consulta[?max=N]  cuerpo = SQL → columnas y filas
//!   GET  /v1/cambios?firma=F                 espera (≤25 s) a que cambie el catálogo
//!   GET  /v1/eventos                         flujo SSE: un evento por cada cambio del
//!                                            catálogo (cambiadas, nuevas, borradas)
//!
//! Las consultas reutilizan sesiones abiertas (máx. `POOL_PER_CONN` libres por
//! conexión; se cierran tras `POOL_IDLE` sin uso).
//!
//! Con TLS (por defecto, ver `tls`) o, si se desactiva, HTTP en claro (solo
//! para una red de confianza, VPN o túnel SSH). Las contraseñas no salen por
//! la red salvo que se active expresamente.

use crate::bridge::{self, Bridge, Event, Handle};
use crate::proto::Msg;
use crate::store::{Catalog, Conn, Secrets};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MAX_BODY: usize = 1 << 20;
const MAX_CLIENTS: usize = 32;
/// Sesiones libres que se guardan por conexión.
const POOL_PER_CONN: usize = 4;
/// Una sesión libre más tiempo que esto se cierra.
const POOL_IDLE: Duration = Duration::from_secs(300);

/// Sesiones abiertas y libres, por conexión (id + huella de sus datos: si la
/// conexión cambia en el catálogo, sus sesiones viejas no se reutilizan).
#[derive(Default)]
struct Pool {
    idle: HashMap<String, Vec<(String, Instant)>>,
}

type SharedPool = Arc<Mutex<Pool>>;

#[derive(Clone)]
pub struct Config {
    pub addr: String,
    pub token: String,
    pub share_passwords: bool,
    pub catalog: PathBuf,
    pub java: bridge::Config,
    pub max_rows: u32,
    /// TLS; `None` = HTTP en claro.
    pub tls: Option<Arc<rustls::ServerConfig>>,
}

type Routes = Arc<Mutex<HashMap<u64, Sender<Result<Msg, String>>>>>;

struct Hub {
    cfg: Config,
    java: Handle,
    routes: Routes,
    clients: AtomicUsize,
    pool: SharedPool,
    stop: Arc<AtomicBool>,
}

/// Servicio en marcha; se para al soltarlo.
pub struct Server {
    stop: Arc<AtomicBool>,
    pub addr: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Arranca en un hilo. Error si no hay token o no se puede escuchar.
pub fn start(cfg: Config) -> Result<Server, String> {
    if cfg.token.trim().len() < 16 {
        return Err("el servicio necesita un token de al menos 16 caracteres".into());
    }
    let listener = TcpListener::bind(&cfg.addr).map_err(|e| format!("no se puede escuchar en {}: {e}", cfg.addr))?;
    let addr = listener.local_addr().map(|a| a.to_string()).unwrap_or(cfg.addr.clone());
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let stop = Arc::new(AtomicBool::new(false));
    let routes: Routes = Arc::default();
    let pool: SharedPool = Arc::default();
    let bridge = Bridge::new(cfg.java.clone(), Arc::new(|| {}));
    let hub = Arc::new(Hub { java: bridge.handle(), cfg, routes: routes.clone(), clients: AtomicUsize::new(0), pool: pool.clone(), stop: stop.clone() });
    let st = stop.clone();
    std::thread::Builder::new().name("servicio-java".into()).spawn(move || route(bridge, routes, pool, st)).map_err(|e| e.to_string())?;
    // Cierra las sesiones que llevan mucho sin usarse.
    let h = hub.clone();
    std::thread::Builder::new()
        .name("servicio-limpieza".into())
        .spawn(move || {
            while !h.stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(5));
                let old: Vec<String> = {
                    let mut p = h.pool.lock().unwrap();
                    let mut out = Vec::new();
                    for list in p.idle.values_mut() {
                        list.retain(|(s, t)| {
                            let keep = t.elapsed() < POOL_IDLE;
                            if !keep {
                                out.push(s.clone());
                            }
                            keep
                        });
                    }
                    out
                };
                for s in old {
                    h.java.send("cerrar", &[("sesion", &s)]);
                }
            }
        })
        .map_err(|e| e.to_string())?;
    let st = stop.clone();
    std::thread::Builder::new()
        .name("servicio".into())
        .spawn(move || {
            while !st.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        if hub.clients.load(Ordering::Relaxed) >= MAX_CLIENTS {
                            // En TLS no se puede contestar sin el saludo: se corta.
                            if hub.cfg.tls.is_none() {
                                let _ = respond(&mut stream, 503, &err_json("demasiadas peticiones a la vez"));
                            }
                            continue;
                        }
                        hub.clients.fetch_add(1, Ordering::Relaxed);
                        let h = hub.clone();
                        std::thread::spawn(move || {
                            let _ = stream.set_nonblocking(false);
                            let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
                            let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
                            match &h.cfg.tls {
                                Some(cfg) => {
                                    if let Ok(conn) = rustls::ServerConnection::new(cfg.clone()) {
                                        let mut s = rustls::StreamOwned::new(conn, stream);
                                        handle(&h, &mut s);
                                        s.conn.send_close_notify();
                                        let _ = s.flush();
                                    }
                                }
                                None => handle(&h, &mut stream),
                            }
                            h.clients.fetch_sub(1, Ordering::Relaxed);
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(50)),
                    Err(_) => std::thread::sleep(Duration::from_millis(200)),
                }
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(Server { stop, addr })
}

/// Reparte las respuestas de Java a la petición HTTP que las espera.
fn route(bridge: Bridge, routes: Routes, pool: SharedPool, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        let Ok(ev) = bridge.rx.recv_timeout(Duration::from_millis(500)) else { continue };
        let mut r = routes.lock().unwrap();
        match ev {
            Event::Msg(m) => {
                if let Some(tx) = r.get(&m.id) {
                    let _ = tx.send(Ok(m));
                }
            }
            Event::Failed(id, e) => {
                if let Some(tx) = r.remove(&id) {
                    let _ = tx.send(Err(e));
                }
            }
            Event::Exited(log) => {
                // Las sesiones murieron con Java.
                pool.lock().unwrap().idle.clear();
                for (_, tx) in r.drain() {
                    let _ = tx.send(Err(format!("Java se cerró: {log}")));
                }
            }
        }
    }
}

impl Hub {
    /// Envía una orden y devuelve el receptor de sus mensajes.
    fn ask(&self, cmd: &str, fields: &[(String, String)]) -> (u64, Receiver<Result<Msg, String>>) {
        let (tx, rx) = mpsc::channel();
        let refs: Vec<(&str, &str)> = fields.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        // Registrar antes de enviar: la respuesta puede llegar enseguida.
        let mut routes = self.routes.lock().unwrap();
        let id = self.java.send(cmd, &refs);
        routes.insert(id, tx);
        (id, rx)
    }

    fn done(&self, id: u64) {
        self.routes.lock().unwrap().remove(&id);
    }

    /// Espera una única respuesta (ok/error).
    fn ask_one(&self, cmd: &str, fields: &[(String, String)], limit: Duration) -> Result<Msg, String> {
        let (id, rx) = self.ask(cmd, fields);
        let r = rx.recv_timeout(limit).map_err(|_| format!("sin respuesta tras {} s", limit.as_secs())).and_then(|r| r);
        self.done(id);
        r
    }

    /// Una sesión de la conexión: libre del pool o recién abierta.
    /// Devuelve (sesión, clave del pool, ¿reutilizada?).
    fn take_session(&self, c: &Conn) -> Result<(String, String, bool), String> {
        use std::hash::{Hash, Hasher};
        let fields = self.fields(c).map_err(|e| err_json(&e))?;
        let mut h = std::collections::hash_map::DefaultHasher::new();
        fields.hash(&mut h);
        let key = format!("{}|{:x}", c.id, h.finish());
        if let Some((s, _)) = self.pool.lock().unwrap().idle.get_mut(&key).and_then(|l| l.pop()) {
            return Ok((s, key, true));
        }
        match self.ask_one("abrir", &fields, Duration::from_secs(c.timeout as u64 + 30)) {
            Ok(m) if &*m.kind == "ok" => Ok((m.get("sesion").unwrap_or("").to_owned(), key, false)),
            Ok(m) => Err(fail_json(&m)),
            Err(e) => Err(err_json(&e)),
        }
    }

    /// Devuelve la sesión al pool (o la cierra si ya hay bastantes libres).
    fn give_back(&self, key: String, session: String) {
        let extra = {
            let mut p = self.pool.lock().unwrap();
            let list = p.idle.entry(key).or_default();
            list.push((session, Instant::now()));
            if list.len() > POOL_PER_CONN { Some(list.remove(0).0) } else { None }
        };
        if let Some(s) = extra {
            self.java.send("cerrar", &[("sesion", &s)]);
        }
    }

    fn idle_sessions(&self) -> usize {
        self.pool.lock().unwrap().idle.values().map(Vec::len).sum()
    }

    fn catalog(&self) -> Catalog {
        Catalog { dir: self.cfg.catalog.clone() }
    }

    fn find(&self, id: &str) -> Option<Conn> {
        self.catalog().load().into_iter().map(|e| e.conn).find(|c| c.id == id)
    }

    fn fields(&self, c: &Conn) -> Result<Vec<(String, String)>, String> {
        let pass = Secrets::load().password_for(c, &self.cfg.catalog).unwrap_or_default();
        bridge::conn_fields(c, &pass, &self.cfg.catalog)
    }
}

// ---------- HTTP ----------

struct Request {
    method: String,
    path: String,
    query: HashMap<String, String>,
    auth: Option<String>,
    body: String,
}

fn read_request<S: Read>(stream: &mut S) -> Result<Request, (u16, String)> {
    let mut r = BufReader::new(stream);
    let mut line = String::new();
    r.read_line(&mut line).map_err(|e| (400, e.to_string()))?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_owned();
    let target = parts.next().unwrap_or("/").to_owned();
    let mut len = 0usize;
    let mut auth = None;
    loop {
        let mut h = String::new();
        if r.read_line(&mut h).map_err(|e| (400, e.to_string()))? == 0 {
            break;
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            match k.trim().to_ascii_lowercase().as_str() {
                "content-length" => len = v.trim().parse().map_err(|_| (400, "Content-Length".to_owned()))?,
                "authorization" => auth = v.trim().strip_prefix("Bearer ").map(|t| t.trim().to_owned()),
                _ => {}
            }
        }
    }
    if len > MAX_BODY {
        return Err((413, "cuerpo demasiado grande".into()));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).map_err(|e| (400, e.to_string()))?;
    let (path, q) = target.split_once('?').unwrap_or((&target, ""));
    let query = q.split('&').filter_map(|kv| kv.split_once('=').or(Some((kv, "")))).filter(|(k, _)| !k.is_empty()).map(|(k, v)| (decode(k), decode(v))).collect();
    Ok(Request { method, path: decode(path), query, auth, body: String::from_utf8_lossy(&body).into_owned() })
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                match u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 3;
                        continue;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            b'+' => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn respond<S: Write>(stream: &mut S, code: u16, body: &str) -> std::io::Result<()> {
    let reason = match code {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        503 => "Service Unavailable",
        _ => "Error",
    };
    write!(
        stream,
        "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}

/// Comparación en tiempo constante (no revela el token por tiempos).
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn handle<S: Read + Write>(hub: &Hub, stream: &mut S) {
    let (code, body) = match read_request(stream) {
        Err((c, e)) => (c, err_json(&e)),
        Ok(req) if !req.auth.as_deref().is_some_and(|t| same(t, hub.cfg.token.trim())) => (401, err_json("falta el token o no es válido (Authorization: Bearer …)")),
        Ok(req) if req.method == "GET" && req.path.trim_end_matches('/') == "/v1/eventos" => return events(hub, stream),
        Ok(req) => dispatch(hub, &req),
    };
    let _ = respond(stream, code, &body);
}

/// Flujo SSE: avisa de cada cambio del catálogo hasta que el cliente se va.
fn events<S: Write>(hub: &Hub, stream: &mut S) {
    let cat = hub.catalog();
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n";
    let send = |s: &mut S, text: &str| s.write_all(text.as_bytes()).and_then(|_| s.flush());
    let mut before = cat.stamps();
    let hello = format!("event: hola\ndata: {{\"firma\":\"{:x}\",\"conexiones\":{}}}\n\n", cat.signature(), before.len());
    if send(stream, head).and_then(|_| send(stream, &hello)).is_err() {
        return;
    }
    let mut last = Instant::now();
    while !hub.stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(500));
        let now = cat.stamps();
        if now != before {
            let list = |v: Vec<&String>| v.into_iter().map(|s| js(s)).collect::<Vec<_>>().join(",");
            let changed = list(now.iter().filter(|(k, v)| before.get(*k).is_some_and(|b| b != *v)).map(|(k, _)| k).collect());
            let added = list(now.keys().filter(|k| !before.contains_key(*k)).collect());
            let removed = list(before.keys().filter(|k| !now.contains_key(*k)).collect());
            let ev = format!(
                "event: cambio\ndata: {{\"firma\":\"{:x}\",\"cambiadas\":[{changed}],\"nuevas\":[{added}],\"borradas\":[{removed}]}}\n\n",
                cat.signature()
            );
            if send(stream, &ev).is_err() {
                return;
            }
            before = now;
            last = Instant::now();
        } else if last.elapsed() > Duration::from_secs(15) {
            // Latido: mantiene viva la conexión y detecta clientes que se fueron.
            if send(stream, ": latido\n\n").is_err() {
                return;
            }
            last = Instant::now();
        }
    }
}

fn dispatch(hub: &Hub, req: &Request) -> (u16, String) {
    let segs: Vec<&str> = req.path.trim_matches('/').split('/').collect();
    match (req.method.as_str(), segs.as_slice()) {
        ("GET", ["v1", "estado"]) => {
            let cat = hub.catalog();
            let n = cat.load().len();
            (200, format!("{{\"version\":{},\"conexiones\":{n},\"firma\":\"{:x}\",\"sesiones_libres\":{}}}", js(env!("CARGO_PKG_VERSION")), cat.signature(), hub.idle_sessions()))
        }
        ("GET", ["v1", "conexiones"]) => {
            let list: Vec<String> = hub.catalog().load().iter().map(|e| conn_json(&e.conn, None)).collect();
            (200, format!("[{}]", list.join(",")))
        }
        ("GET", ["v1", "conexiones", id]) => match hub.find(id) {
            None => (404, err_json("no existe esa conexión")),
            Some(c) => {
                if req.query.get("clave").is_some_and(|v| v == "1") {
                    if !hub.cfg.share_passwords {
                        return (403, err_json("este servicio no entrega contraseñas (servir_claves=no)"));
                    }
                    let p = Secrets::load().password_for(&c, &hub.cfg.catalog).unwrap_or_default();
                    (200, conn_json(&c, Some(&p)))
                } else {
                    (200, conn_json(&c, None))
                }
            }
        },
        ("POST", ["v1", "conexiones", id, "probar"]) => match hub.find(id) {
            None => (404, err_json("no existe esa conexión")),
            Some(c) => {
                let limit = Duration::from_secs(c.timeout as u64 + 30);
                match hub.fields(&c).and_then(|f| hub.ask_one("probar", &f, limit)) {
                    Ok(m) if &*m.kind == "ok" => (200, ok_json(&m)),
                    Ok(m) => (200, fail_json(&m)),
                    Err(e) => (200, err_json(&e)),
                }
            }
        },
        ("POST", ["v1", "conexiones", id, "consulta"]) => match hub.find(id) {
            None => (404, err_json("no existe esa conexión")),
            Some(c) => {
                let max: u32 = req.query.get("max").and_then(|m| m.parse().ok()).unwrap_or(hub.cfg.max_rows);
                (200, query(hub, &c, req.body.trim().trim_end_matches(';'), max))
            }
        },
        ("GET", ["v1", "cambios"]) => {
            let cat = hub.catalog();
            let from = req.query.get("firma").cloned().unwrap_or_default();
            let start = Instant::now();
            loop {
                let sig = format!("{:x}", cat.signature());
                if sig != from || start.elapsed() > Duration::from_secs(25) {
                    return (200, format!("{{\"firma\":\"{sig}\",\"cambio\":{}}}", sig != from));
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        }
        (_, ["v1", ..]) => (404, err_json("ruta desconocida; ver /v1/estado, /v1/conexiones")),
        _ => (404, err_json("ruta desconocida")),
    }
}

/// Ejecuta en una sesión del pool; si la reutilizada ya no vale (Java se
/// reinició), lo intenta una vez con una nueva.
fn query(hub: &Hub, c: &Conn, sql: &str, max: u32) -> String {
    for attempt in 0..2 {
        let (session, key, reused) = match hub.take_session(c) {
            Ok(x) => x,
            Err(json) => return json,
        };
        match run_query(hub, &session, sql, max) {
            Ok(json) => {
                hub.give_back(key, session);
                return json;
            }
            Err((json, broken)) => {
                if broken {
                    hub.java.send("cerrar", &[("sesion", &session)]);
                    if reused && attempt == 0 {
                        continue;
                    }
                } else {
                    // Error de SQL: la sesión sigue sirviendo.
                    hub.give_back(key, session);
                }
                return json;
            }
        }
    }
    err_json("no se pudo abrir una sesión")
}

/// Las filas se van escribiendo en el JSON. Error: (json, ¿sesión inservible?).
fn run_query(hub: &Hub, session: &str, sql: &str, max: u32) -> Result<String, (String, bool)> {
    let (id, rx) = hub.ask("consulta", &[("sesion".into(), session.to_owned()), ("sql".into(), sql.into()), ("max".into(), max.to_string())]);
    let mut out = String::from("{\"ok\":true");
    let mut rows = 0usize;
    let result = loop {
        match rx.recv_timeout(Duration::from_secs(300)) {
            Err(_) => break Err((err_json("sin respuesta tras 300 s"), true)),
            Ok(Err(e)) => break Err((err_json(&e), true)),
            Ok(Ok(m)) => match &*m.kind {
                "columnas" => {
                    let cols: Vec<String> = m
                        .fields
                        .iter()
                        .map(|f| {
                            let f = f.as_deref().unwrap_or("");
                            let (n, t) = f.split_once('\u{1}').unwrap_or((f, ""));
                            format!("{{\"nombre\":{},\"tipo\":{}}}", js(n), js(t))
                        })
                        .collect();
                    out.push_str(&format!(",\"columnas\":[{}],\"filas\":[", cols.join(",")));
                }
                "fila" => {
                    if rows > 0 {
                        out.push(',');
                    }
                    rows += 1;
                    out.push('[');
                    for (i, f) in m.fields.iter().enumerate() {
                        if i > 0 {
                            out.push(',');
                        }
                        out.push_str(&f.as_deref().map_or("null".to_owned(), js));
                    }
                    out.push(']');
                }
                "fin" => {
                    if out.contains("\"filas\":[") {
                        out.push(']');
                    }
                    out.push_str(&format!(
                        ",\"mas\":{},\"actualizadas\":{},\"ms\":{}}}",
                        m.get("mas") == Some("true"),
                        js(m.get("actualizadas").unwrap_or("")),
                        m.get("ms").unwrap_or("0")
                    ));
                    break Ok(());
                }
                "error" => {
                    // Sesión perdida o conexión caída (SQLState 08…): no reutilizar.
                    let broken = m.get("mensaje").is_some_and(|x| x.contains("la sesión no existe")) || m.get("sqlstate").is_some_and(|x| x.starts_with("08"));
                    break Err((fail_json(&m), broken));
                }
                _ => {}
            },
        }
    };
    hub.done(id);
    result.map(|_| out)
}

// ---------- JSON ----------

pub fn js(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for ch in s.chars() {
        match ch {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn err_json(msg: &str) -> String {
    format!("{{\"ok\":false,\"error\":{{\"mensaje\":{}}}}}", js(msg))
}

fn ok_json(m: &Msg) -> String {
    let pairs: Vec<String> = m.pairs().filter(|(k, _)| *k != "sesion").map(|(k, v)| format!("{}:{}", js(k), js(v))).collect();
    format!("{{\"ok\":true,\"info\":{{{}}}}}", pairs.join(","))
}

fn fail_json(m: &Msg) -> String {
    let pairs: Vec<String> = m.pairs().map(|(k, v)| format!("{}:{}", js(k), js(v))).collect();
    format!("{{\"ok\":false,\"error\":{{{}}}}}", pairs.join(","))
}

pub fn conn_json(c: &Conn, password: Option<&str>) -> String {
    let jars: Vec<String> = c.jars.iter().map(|j| js(j)).collect();
    let props: Vec<String> = c.props.iter().map(|(k, v)| format!("{}:{}", js(k), js(v))).collect();
    let mut s = format!(
        "{{\"id\":{},\"nombre\":{},\"grupo\":{},\"url\":{},\"usuario\":{},\"driver\":{},\"jars\":[{}],\"propiedades\":{{{}}},\"timeout\":{},\"notas\":{},\"clave_compartida\":{}",
        js(&c.id),
        js(&c.name),
        js(&c.group),
        js(&c.url),
        js(&c.user),
        js(&c.driver),
        jars.join(","),
        props.join(","),
        c.timeout,
        js(&c.notes),
        !c.secret.is_empty()
    );
    if let Some(p) = password {
        s.push_str(&format!(",\"clave\":{}", js(p)));
    }
    s.push('}');
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpStream;

    fn http(addr: &str, req: &str) -> (u16, String) {
        let mut s = TcpStream::connect(addr).unwrap();
        s.write_all(req.as_bytes()).unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        let code = out[9..12].parse().unwrap();
        (code, out.split_once("\r\n\r\n").unwrap().1.to_owned())
    }

    #[test]
    fn json_escapes() {
        assert_eq!(js("a\"b\\c\nd\u{1}"), "\"a\\\"b\\\\c\\nd\\u0001\"");
        assert_eq!(decode("a%20b+c%C3%B1"), "a b cñ");
        assert_eq!(decode("100%"), "100%");
    }

    #[test]
    fn serves_catalog_with_token() {
        let dir = crate::store::temp_dir("servicio");
        let cat = Catalog { dir: dir.clone() };
        let c = Conn { id: "uno".into(), name: "Uno \"1\"".into(), url: "jdbc:h2:mem:x".into(), secret: "x1:zz".into(), ..Conn::new() };
        cat.save(&c, None).unwrap();
        let cfg = Config {
            addr: "127.0.0.1:0".into(),
            token: "0123456789abcdef0123".into(),
            share_passwords: false,
            catalog: dir.clone(),
            java: bridge::Config::default(),
            max_rows: 10,
            tls: None,
        };
        assert!(start(Config { token: "corto".into(), ..cfg.clone() }).is_err());
        let srv = start(cfg).unwrap();
        let a = srv.addr.clone();
        let (code, _) = http(&a, "GET /v1/conexiones HTTP/1.1\r\n\r\n");
        assert_eq!(code, 401);
        let (code, _) = http(&a, "GET /v1/conexiones HTTP/1.1\r\nAuthorization: Bearer 0123456789abcdef0124\r\n\r\n");
        assert_eq!(code, 401);
        let auth = "Authorization: Bearer 0123456789abcdef0123\r\n";
        let (code, body) = http(&a, &format!("GET /v1/conexiones HTTP/1.1\r\n{auth}\r\n"));
        assert_eq!(code, 200);
        assert!(body.starts_with("[{\"id\":\"uno\",\"nombre\":\"Uno \\\"1\\\"\""), "{body}");
        assert!(body.contains("\"clave_compartida\":true") && !body.contains("x1:zz"), "{body}");
        let (code, _) = http(&a, &format!("GET /v1/conexiones/uno?clave=1 HTTP/1.1\r\n{auth}\r\n"));
        assert_eq!(code, 403);
        let (code, _) = http(&a, &format!("GET /v1/conexiones/nada HTTP/1.1\r\n{auth}\r\n"));
        assert_eq!(code, 404);
        let (code, body) = http(&a, &format!("GET /v1/estado HTTP/1.1\r\n{auth}\r\n"));
        assert!(code == 200 && body.contains("\"conexiones\":1"), "{body}");
        drop(srv);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// HTTPS con certificado autofirmado: un cliente que confía en ese
    /// certificado conecta; uno que no lo conoce es rechazado.
    #[test]
    fn serves_over_tls() {
        let dir = crate::store::temp_dir("servicio-tls");
        let (cert, key) = (dir.join("c.pem"), dir.join("k.pem"));
        crate::tls::create_self_signed(&cert, &key).unwrap();
        let t = crate::tls::load(cert.to_str().unwrap(), key.to_str().unwrap()).unwrap();
        assert_eq!(t.fingerprint.len(), 32 * 3 - 1);
        let cfg = Config {
            addr: "127.0.0.1:0".into(),
            token: "0123456789abcdef0123".into(),
            share_passwords: false,
            catalog: dir.clone(),
            java: bridge::Config::default(),
            max_rows: 10,
            tls: Some(t.config.clone()),
        };
        let srv = start(cfg).unwrap();
        let client = |roots: rustls::RootCertStore| -> Result<String, String> {
            let cc = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_root_certificates(roots)
                .with_no_client_auth();
            let conn = rustls::ClientConnection::new(Arc::new(cc), "localhost".try_into().unwrap()).unwrap();
            let mut s = rustls::StreamOwned::new(conn, TcpStream::connect(&srv.addr).unwrap());
            s.write_all(b"GET /v1/estado HTTP/1.1\r\nAuthorization: Bearer 0123456789abcdef0123\r\n\r\n").map_err(|e| e.to_string())?;
            let mut out = String::new();
            match s.read_to_string(&mut out) {
                Ok(_) => Ok(out),
                // Algunos servidores cierran sin close_notify; con respuesta completa vale.
                Err(e) if !out.is_empty() => Ok(format!("{out}{e:?}")),
                Err(e) => Err(e.to_string()),
            }
        };
        let mut roots = rustls::RootCertStore::empty();
        for c in rustls::pki_types::CertificateDer::pem_file_iter(&cert).unwrap() {
            roots.add(c.unwrap()).unwrap();
        }
        use rustls::pki_types::pem::PemObject;
        let ok = client(roots).unwrap();
        assert!(ok.starts_with("HTTP/1.1 200") && ok.contains("\"conexiones\":0"), "{ok}");
        assert!(client(rustls::RootCertStore::empty()).is_err(), "sin confiar en el certificado debe fallar");
        drop(srv);
        let _ = std::fs::remove_dir_all(dir);
    }
}

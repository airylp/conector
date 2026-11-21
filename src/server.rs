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
    let bridge = Bridge::new(cfg.java.clone(), Arc::new(|| {}));
    let hub = Arc::new(Hub { java: bridge.handle(), cfg, routes: routes.clone(), clients: AtomicUsize::new(0) });
    let st = stop.clone();
    std::thread::Builder::new().name("servicio-java".into()).spawn(move || route(bridge, routes, st)).map_err(|e| e.to_string())?;
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
fn route(bridge: Bridge, routes: Routes, stop: Arc<AtomicBool>) {
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

    fn catalog(&self) -> Catalog {
        Catalog { dir: self.cfg.catalog.clone() }
    }

    fn find(&self, id: &str) -> Option<Conn> {
        self.catalog().load().into_iter().map(|e| e.conn).find(|c| c.id == id)
    }

    fn fields(&self, c: &Conn) -> Vec<(String, String)> {
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
        Ok(req) => dispatch(hub, &req),
    };
    let _ = respond(stream, code, &body);
}

fn dispatch(hub: &Hub, req: &Request) -> (u16, String) {
    let segs: Vec<&str> = req.path.trim_matches('/').split('/').collect();
    match (req.method.as_str(), segs.as_slice()) {
        ("GET", ["v1", "estado"]) => {
            let cat = hub.catalog();
            let n = cat.load().len();
            (200, format!("{{\"version\":{},\"conexiones\":{n},\"firma\":\"{:x}\"}}", js(env!("CARGO_PKG_VERSION")), cat.signature()))
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
                match hub.ask_one("probar", &hub.fields(&c), limit) {
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

/// Abre una sesión, ejecuta y la cierra; las filas se van escribiendo en el JSON.
fn query(hub: &Hub, c: &Conn, sql: &str, max: u32) -> String {
    let open = match hub.ask_one("abrir", &hub.fields(c), Duration::from_secs(c.timeout as u64 + 30)) {
        Ok(m) if &*m.kind == "ok" => m,
        Ok(m) => return fail_json(&m),
        Err(e) => return err_json(&e),
    };
    let session = open.get("sesion").unwrap_or("").to_owned();
    let (id, rx) = hub.ask("consulta", &[("sesion".into(), session.clone()), ("sql".into(), sql.into()), ("max".into(), max.to_string())]);
    let mut out = String::from("{\"ok\":true");
    let mut rows = 0usize;
    let result = loop {
        match rx.recv_timeout(Duration::from_secs(300)) {
            Err(_) => break Err("sin respuesta tras 300 s".to_owned()),
            Ok(Err(e)) => break Err(e),
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
                "error" => break Err(fail_json(&m)),
                _ => {}
            },
        }
    };
    hub.done(id);
    let _ = hub.java.send("cerrar", &[("sesion", &session)]);
    match result {
        Ok(()) => out,
        Err(e) if e.starts_with('{') => e,
        Err(e) => err_json(&e),
    }
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

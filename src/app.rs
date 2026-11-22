//! Interfaz: lista de conexiones (catálogo), formulario, prueba de conexión,
//! consola SQL con explorador de esquemas y resultados virtualizados (solo se
//! dibujan las filas visibles; las celdas se guardan como `Box<str>`).
//! También: frase común (contraseñas compartidas cifradas), descargas (Java
//! portátil, drivers de Maven) y el servicio de red para otros programas.

use crate::bridge::{self, Bridge, Event};
use crate::crypto::{self, Key as CKey, Lock};
use crate::i18n::{self, Lang};
use crate::javadl::{self, State as Dl};
use crate::proto::Msg;
use crate::server;
use crate::store::{self, Catalog, Conn, Entry, SaveError, Secrets, Settings};
use eframe::egui::*;
use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// Plantillas: nombre, clase del driver, URL de ejemplo, prefijo de URL.
pub const TEMPLATES: &[(&str, &str, &str, &str)] = &[
    ("PostgreSQL", "org.postgresql.Driver", "jdbc:postgresql://servidor:5432/basedatos", "jdbc:postgresql:"),
    ("MySQL", "com.mysql.cj.jdbc.Driver", "jdbc:mysql://servidor:3306/basedatos", "jdbc:mysql:"),
    ("MariaDB", "org.mariadb.jdbc.Driver", "jdbc:mariadb://servidor:3306/basedatos", "jdbc:mariadb:"),
    ("Oracle", "oracle.jdbc.OracleDriver", "jdbc:oracle:thin:@//servidor:1521/servicio", "jdbc:oracle:"),
    ("SQL Server", "com.microsoft.sqlserver.jdbc.SQLServerDriver", "jdbc:sqlserver://servidor:1433;databaseName=basedatos;encrypt=true;trustServerCertificate=true", "jdbc:sqlserver:"),
    ("DB2", "com.ibm.db2.jcc.DB2Driver", "jdbc:db2://servidor:50000/basedatos", "jdbc:db2:"),
    ("Firebird", "org.firebirdsql.jdbc.FBDriver", "jdbc:firebirdsql://servidor:3050/ruta/base.fdb", "jdbc:firebirdsql:"),
    ("H2", "org.h2.Driver", "jdbc:h2:mem:prueba", "jdbc:h2:"),
    ("SQLite", "org.sqlite.JDBC", "jdbc:sqlite:/ruta/archivo.db", "jdbc:sqlite:"),
];

/// Plantilla que corresponde a una URL.
fn template_for(url: &str) -> Option<&'static str> {
    TEMPLATES.iter().find(|t| url.trim().starts_with(t.3)).map(|t| t.0)
}

/// Cada cuánto se mira si otro equipo cambió el catálogo.
const SCAN_EVERY: Duration = Duration::from_secs(2);
const GREEN: Color32 = Color32::from_rgb(60, 170, 90);
const RED: Color32 = Color32::from_rgb(210, 70, 60);
const ORANGE: Color32 = Color32::from_rgb(220, 140, 30);

#[derive(Clone)]
struct ErrInfo {
    msg: String,
    detail: String,
}

impl ErrInfo {
    fn from_msg(m: &Msg) -> ErrInfo {
        let mut detail = String::new();
        for (k, v) in m.pairs() {
            match k {
                "mensaje" => {}
                "causas" => detail.push_str(&format!("{}\n{v}\n", t!("Causas:"))),
                "siguientes" => detail.push_str(&format!("{}\n{v}\n", t!("Errores encadenados:"))),
                k => detail.push_str(&format!("{k}: {v}\n")),
            }
        }
        ErrInfo { msg: m.get("mensaje").unwrap_or("error").into(), detail: detail.trim_end().into() }
    }
    fn plain(msg: impl Into<String>) -> ErrInfo {
        ErrInfo { msg: msg.into(), detail: String::new() }
    }
}

enum TestState {
    Idle,
    Running { id: u64, since: Instant, limit: Duration },
    Ok(Vec<(String, String)>),
    Err(ErrInfo),
}

/// Dónde se guarda la contraseña.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Keep {
    No,
    /// Solo en este equipo (`secretos.txt`).
    Local,
    /// En el catálogo, cifrada con la frase común.
    Shared,
}

impl Keep {
    fn label(self) -> &'static str {
        match self {
            Keep::No => t!("No guardar"),
            Keep::Local => t!("Solo en este equipo"),
            Keep::Shared => t!("Compartida (cifrada)"),
        }
    }
}

/// Formulario de la conexión seleccionada (textos tal cual se editan).
struct Form {
    conn: Conn,
    jars: String,
    props: String,
    password: String,
    keep: Keep,
    /// Lo que hay guardado (para saber si hay cambios).
    saved: (Conn, String, Keep),
    /// Fecha del archivo al leerlo; `None` = conexión nueva sin archivo.
    mtime: Option<Option<SystemTime>>,
    /// Otro equipo cambió el archivo mientras se editaba.
    external: bool,
    /// Siguiente «Guardar» sobrescribe aunque haya cambiado fuera.
    force: bool,
    /// Tiene contraseña compartida pero falta la frase para descifrarla.
    locked: bool,
}

impl Form {
    fn from_entry(e: &Entry, secrets: &Secrets, catalog: &std::path::Path, key: Option<&CKey>) -> Form {
        let c = e.conn.clone();
        let (pass, keep, locked) = if let Some(p) = secrets.get(&Secrets::key(catalog, &c.id)) {
            (p.to_owned(), Keep::Local, false)
        } else if !c.secret.is_empty() {
            match key.and_then(|k| crypto::decrypt(k, &c.secret)) {
                Some(p) => (p, Keep::Shared, false),
                None => (String::new(), Keep::Shared, true),
            }
        } else {
            (String::new(), Keep::No, false)
        };
        Form {
            jars: c.jars.join("\n"),
            props: c.props.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("\n"),
            saved: (c.clone(), pass.clone(), keep),
            conn: c,
            password: pass,
            keep,
            mtime: Some(e.mtime),
            external: false,
            force: false,
            locked,
        }
    }

    fn new_conn(name: String) -> Form {
        let c = Conn { name, ..Conn::new() };
        Form {
            jars: String::new(),
            props: String::new(),
            saved: (Conn::new(), String::new(), Keep::No),
            conn: c,
            password: String::new(),
            keep: Keep::No,
            mtime: None,
            external: false,
            force: false,
            locked: false,
        }
    }

    /// La conexión con jar y propiedades tomados de los textos.
    fn built(&self) -> Conn {
        let mut c = self.conn.clone();
        c.jars = self.jars.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect();
        c.props = self
            .props
            .lines()
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
            .filter(|(k, _)| !k.is_empty())
            .collect();
        c
    }

    fn dirty(&self) -> bool {
        self.mtime.is_none() || self.built() != self.saved.0 || self.keep != self.saved.2 || (self.keep != Keep::No && self.password != self.saved.1)
    }
}

/// Un resultado de la consola (una pestaña).
#[derive(Default)]
struct ResultTab {
    title: String,
    /// Sentencia que lo produjo (para volver a ejecutarla al exportar).
    sql: String,
    cols: Vec<(String, String)>,
    rows: Vec<Box<[Option<Box<str>>]>>,
    widths: Vec<f32>,
    info: String,
    /// Pestaña de texto (DDL) en vez de tabla.
    text: Option<String>,
}

/// Máximo de pestañas de resultados (las más viejas se cierran).
const MAX_TABS: usize = 30;

#[derive(Default)]
struct Console {
    /// Sesión abierta en Java: (número, nombre de la conexión, id de la conexión).
    session: Option<(String, String)>,
    session_conn: String,
    opening: Option<u64>,
    /// Ejecutar la cola en cuanto se abra la sesión.
    queued: bool,
    sql: String,
    /// Sentencias pendientes del script en curso.
    queue: std::collections::VecDeque<String>,
    /// Sentencia en curso: (id, inicio, texto, ¿ya tiene pestaña?).
    running: Option<(u64, Instant)>,
    current: String,
    current_tab: bool,
    tabs: Vec<ResultTab>,
    /// Pestaña visible; `None` = Mensajes.
    active: Option<usize>,
    /// Mensajes de la ejecución: (¿bien?, texto).
    log: Vec<(bool, String)>,
    /// Información de la sesión (conectado en…).
    info: String,
    error: Option<ErrInfo>,
    /// Seguir con el script aunque una sentencia falle.
    keep_going: bool,
    /// Cursor y selección del editor (bytes), del fotograma anterior.
    cursor: usize,
    selection: Option<std::ops::Range<usize>>,
    show_history: bool,
    history_filter: String,
}

impl Console {
    fn tab(&mut self) -> Option<&mut ResultTab> {
        self.active.and_then(|i| self.tabs.get_mut(i))
    }

    fn push_tab(&mut self, t: ResultTab) {
        if self.tabs.len() >= MAX_TABS {
            self.tabs.remove(0);
        }
        self.tabs.push(t);
        self.active = Some(self.tabs.len() - 1);
    }
}

/// Título corto de una sentencia para su pestaña.
fn short_title(sql: &str) -> String {
    let one: String = crate::sql::strip_leading_comments(sql).split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() > 40 { format!("{}…", one.chars().take(39).collect::<String>()) } else { one }
}

/// Explorador de esquemas de la sesión abierta.
#[derive(Default)]
struct Explorer {
    schemas: Vec<Schema>,
    loading: bool,
    error: Option<String>,
    filter: String,
}

struct Schema {
    cat: String,
    sch: String,
    tables: Option<Vec<(String, String)>>,
    loading: bool,
    open: bool,
}

impl Schema {
    fn label(&self) -> &str {
        if self.sch.is_empty() { &self.cat } else { &self.sch }
    }
    /// Prefijo para el SQL: `esquema.` (o `catálogo.` en bases sin esquemas).
    fn qualifier(&self) -> &str {
        self.label()
    }
}

enum Meta {
    Schemas,
    Tables(String, String),
    Columns(String),
    /// DDL de una tabla: (nombre completo, tipo de getTables).
    Ddl(String, String),
}

enum Pending {
    /// Exportación sin límite en curso.
    Export,
    /// Un lote de la importación.
    Import,
    Test,
    Open,
    Query,
    Meta(Meta),
    Other,
}

enum Picked {
    ExportPath(std::path::PathBuf),
    ImportPath(std::path::PathBuf),
    Catalog(std::path::PathBuf),
    Jar(Vec<std::path::PathBuf>),
}

/// Ventana «Exportar».
struct ExportDialog {
    fmt: crate::export::Format,
    /// Volver a ejecutar sin límite (si no, solo las filas cargadas).
    all: bool,
    table: String,
    path: String,
    tab: usize,
}

/// Exportación en curso (filas que llegan de Java directas al archivo).
struct ExportJob {
    id: u64,
    exporter: Option<crate::export::Exporter>,
    path: std::path::PathBuf,
    started: Instant,
}

/// Ventana «Importar CSV».
#[derive(Default)]
struct ImportDialog {
    path: String,
    table: String,
    sep: u8,
    header: bool,
    empty_null: bool,
    /// Columnas de destino separadas por comas (por defecto, la cabecera del CSV).
    cols: String,
    preview: Vec<Vec<String>>,
    error: Option<String>,
}

/// Importación en curso: lotes de `IMPORT_BATCH` filas, uno tras otro.
struct ImportJob {
    reader: crate::export::CsvReader<std::fs::File>,
    table: String,
    cols: Vec<String>,
    empty_null: bool,
    inflight: Option<(u64, usize)>,
    done_rows: u64,
    started: Instant,
    eof: bool,
}

const IMPORT_BATCH: usize = 500;

/// Tabla de `select … from X` (para el INSERT exportado).
fn guess_table(sql: &str) -> String {
    let lower = sql.to_lowercase();
    lower.find(" from ").and_then(|i| sql[i + 6..].split_whitespace().next()).map(|t| t.trim_end_matches([';', ')', ',']).to_owned()).unwrap_or_else(|| "tabla".into())
}

/// Campos de la frase común en Ajustes.
#[derive(Default)]
struct PassEdit {
    /// Texto de `variables.txt` mientras se edita en Ajustes.
    vars: String,
    unlock: String,
    new1: String,
    new2: String,
    remember: bool,
}

pub struct App {
    settings: Settings,
    catalog: Catalog,
    secrets: Secrets,
    lock: Option<Lock>,
    key: Option<CKey>,
    entries: Vec<Entry>,
    signature: u64,
    last_scan: Instant,
    selected: Option<String>,
    form: Option<Form>,
    filter: String,
    bridge: Bridge,
    java: Option<String>,
    java_ok: bool,
    java_log: String,
    pending: HashMap<u64, Pending>,
    meta_buf: HashMap<u64, (Vec<String>, Vec<Box<[Option<Box<str>>]>>)>,
    test: TestState,
    /// Nombre de la conexión probada (título de la ventana del resultado).
    test_name: String,
    console: Console,
    explorer: Explorer,
    status: String,
    show_settings: bool,
    settings_edit: Settings,
    pass_edit: PassEdit,
    confirm_delete: bool,
    dialog: (Sender<Picked>, Receiver<Picked>),
    java_dl: Option<javadl::Shared>,
    driver_dl: Option<(javadl::Shared, bool)>,
    server: Option<server::Server>,
    server_err: Option<String>,
    /// Certificado del servicio (ruta, huella) si va con TLS.
    server_cert: Option<(String, String)>,
    debug_frames: u32,
    export_dialog: Option<ExportDialog>,
    export_job: Option<ExportJob>,
    import_dialog: Option<ImportDialog>,
    import_job: Option<ImportJob>,
    /// Comprobación/instalación de versión nueva; `show_updater` = ventana visible.
    updater: Option<crate::update::Updater>,
    show_updater: bool,
}

fn new_bridge(ctx: &Context, s: &Settings) -> Bridge {
    let ctx = ctx.clone();
    Bridge::new(bridge::Config { java: s.java.clone(), jvm_opts: s.jvm.clone() }, Arc::new(move || ctx.request_repaint()))
}

fn apply_lang(s: &Settings) {
    let l = std::env::var("CONECTOR_IDIOMA").ok().or(s.lang.clone()).and_then(|c| Lang::from_code(&c)).or_else(i18n::system).unwrap_or(Lang::Es);
    i18n::set(l);
}

fn apply_theme(ctx: &Context, s: &Settings) {
    ctx.set_theme(match s.dark {
        Some(true) => ThemePreference::Dark,
        Some(false) => ThemePreference::Light,
        None => ThemePreference::System,
    });
}

impl App {
    pub fn new(cc: &eframe::CreationContext) -> App {
        let settings = Settings::load();
        apply_lang(&settings);
        apply_theme(&cc.egui_ctx, &settings);
        let catalog = Catalog { dir: settings.catalog_dir() };
        let _ = std::fs::create_dir_all(&catalog.dir);
        let entries = catalog.load();
        let mut app = App {
            bridge: new_bridge(&cc.egui_ctx, &settings),
            java_ok: bridge::java_available(&settings.java) || std::env::var_os("CONECTOR_AUXILIAR").is_some(),
            signature: catalog.signature(),
            settings_edit: settings.clone(),
            settings,
            catalog,
            secrets: Secrets::load(),
            lock: None,
            key: None,
            entries,
            last_scan: Instant::now(),
            selected: None,
            form: None,
            filter: String::new(),
            java: None,
            java_log: String::new(),
            pending: HashMap::new(),
            meta_buf: HashMap::new(),
            test: TestState::Idle,
            test_name: String::new(),
            console: Console { sql: "select 1".into(), ..Default::default() },
            explorer: Explorer::default(),
            status: String::new(),
            show_settings: false,
            pass_edit: PassEdit { remember: true, ..Default::default() },
            confirm_delete: false,
            dialog: std::sync::mpsc::channel(),
            java_dl: None,
            driver_dl: None,
            server: None,
            server_err: None,
            server_cert: None,
            debug_frames: 0,
            export_dialog: None,
            export_job: None,
            import_dialog: None,
            import_job: None,
            updater: None,
            show_updater: false,
        };
        app.load_lock();
        app.start_server();
        // Aviso discreto: solo si se instaló con install.sh (lleva commit) y no en pruebas.
        if app.settings.check_updates && crate::update::SUPPORTED && crate::update::current_commit().is_some() && std::env::var_os("CONECTOR_CAPTURA").is_none() {
            app.updater = Some(crate::update::Updater::check(cc.egui_ctx.clone()));
        }
        app
    }

    /// Lee `cifrado.txt` del catálogo y la clave recordada en este equipo.
    fn load_lock(&mut self) {
        self.lock = Lock::load(&self.catalog.dir);
        self.key = match &self.lock {
            Some(l) => self.secrets.catalog_key(&self.catalog.dir).filter(|k| l.verify(k)),
            None => None,
        };
    }

    fn start_server(&mut self) {
        self.server = None;
        self.server_err = None;
        self.server_cert = None;
        if !self.settings.serve {
            return;
        }
        let tls = if self.settings.serve_tls {
            match crate::tls::load(&self.settings.serve_cert, &self.settings.serve_key) {
                Ok(t) => Some(t),
                Err(e) => {
                    self.server_err = Some(format!("TLS: {e}"));
                    return;
                }
            }
        } else {
            None
        };
        self.server_cert = tls.as_ref().map(|t| (t.cert_path.display().to_string(), t.fingerprint.clone()));
        if self.settings.serve_token.trim().len() < 16 {
            self.settings.serve_token = store::new_token();
            self.settings_edit.serve_token = self.settings.serve_token.clone();
            let _ = self.settings.save();
        }
        let s = &self.settings;
        let cfg = server::Config {
            addr: s.serve_addr.clone(),
            token: s.serve_token.clone(),
            share_passwords: s.serve_passwords,
            catalog: self.catalog.dir.clone(),
            java: bridge::Config { java: s.java.clone(), jvm_opts: s.jvm.clone() },
            max_rows: s.max_rows,
            tls: tls.map(|t| t.config),
        };
        // El servicio anterior suelta el puerto en ~50 ms.
        for attempt in 0..5 {
            match server::start(cfg.clone()) {
                Ok(srv) => {
                    self.server = Some(srv);
                    self.server_err = None;
                    return;
                }
                Err(e) => {
                    self.server_err = Some(e);
                    if attempt < 4 {
                        std::thread::sleep(Duration::from_millis(60));
                    }
                }
            }
        }
    }

    fn select(&mut self, id: String) {
        let Some(e) = self.entries.iter().find(|e| e.conn.id == id) else { return };
        self.form = Some(Form::from_entry(e, &self.secrets, &self.catalog.dir, self.key.as_ref()));
        self.selected = Some(id);
        self.test = TestState::Idle;
        self.confirm_delete = false;
    }

    fn reload(&mut self) {
        self.entries = self.catalog.load();
        self.signature = self.catalog.signature();
        if Lock::load(&self.catalog.dir) != self.lock {
            self.load_lock();
        }
        let Some(id) = self.selected.clone() else { return };
        match self.entries.iter().find(|e| e.conn.id == id) {
            Some(e) => {
                let changed = self.form.as_ref().is_some_and(|f| f.mtime.is_some_and(|m| m != e.mtime));
                if changed {
                    if self.form.as_ref().is_some_and(|f| f.dirty()) {
                        if let Some(f) = &mut self.form {
                            f.external = true;
                        }
                    } else {
                        self.select(id);
                    }
                }
            }
            None => {
                // Lo borraron en otro equipo: queda como nueva (sin archivo) si se editaba.
                if let Some(f) = &mut self.form {
                    if f.mtime.is_some() {
                        f.mtime = None;
                        f.external = true;
                    }
                }
            }
        }
    }

    fn new_connection(&mut self) {
        let mut f = Form::new_conn(t!("Nueva conexión").into());
        f.conn.id = self.catalog.free_id(&f.conn.name);
        self.selected = Some(f.conn.id.clone());
        self.form = Some(f);
        self.test = TestState::Idle;
    }

    fn duplicate(&mut self) {
        let Some(f) = &self.form else { return };
        let mut c = f.built();
        c.name = tf!("{nombre} (copia)", nombre = c.name);
        c.id = self.catalog.free_id(&c.name);
        let mut nf = Form::new_conn(c.name.clone());
        nf.jars = c.jars.join("\n");
        nf.props = f.props.clone();
        nf.password = f.password.clone();
        nf.keep = f.keep;
        nf.locked = f.locked;
        nf.conn = c;
        self.selected = Some(nf.conn.id.clone());
        self.form = Some(nf);
        self.test = TestState::Idle;
    }

    fn save(&mut self) {
        let Some(f) = &mut self.form else { return };
        let mut c = f.built();
        if c.name.trim().is_empty() {
            self.status = t!("Ponle un nombre a la conexión.").into();
            return;
        }
        c.name = c.name.trim().into();
        if f.mtime.is_none() && self.catalog.path_of(&c.id).exists() {
            c.id = self.catalog.free_id(&c.name);
        }
        // Contraseña: compartida cifrada, local o ninguna.
        let local = Secrets::key(&self.catalog.dir, &c.id);
        let local_pass = match f.keep {
            Keep::No => {
                c.secret.clear();
                None
            }
            Keep::Local => {
                c.secret.clear();
                Some(f.password.clone())
            }
            Keep::Shared => {
                let unchanged = f.saved.2 == Keep::Shared && f.password == f.saved.1 && !c.secret.is_empty();
                if f.locked && f.password.is_empty() || unchanged {
                    // Se conserva la cifrada tal cual (sin la frase no se puede rehacer).
                } else if let Some(k) = &self.key {
                    c.secret = crypto::encrypt(k, &f.password);
                } else {
                    self.status = t!("Para compartir la contraseña crea o desbloquea la frase común en Ajustes.").into();
                    return;
                }
                None
            }
        };
        let expected = if f.force { None } else { Some(f.mtime.unwrap_or(None)) };
        match self.catalog.save(&c, expected) {
            Ok(m) => {
                if let Err(e) = self.secrets.set(local, local_pass) {
                    self.status = tf!("Guardada, pero no la contraseña: {e}", e = e);
                } else {
                    self.status = tf!("Guardada en {ruta}", ruta = self.catalog.path_of(&c.id).display());
                }
                if f.keep == Keep::Shared && !c.secret.is_empty() && self.key.is_some() {
                    f.locked = false;
                }
                f.saved = (c.clone(), f.password.clone(), f.keep);
                f.conn = c.clone();
                f.mtime = Some(m);
                f.external = false;
                f.force = false;
                self.selected = Some(c.id);
                self.entries = self.catalog.load();
                self.signature = self.catalog.signature();
            }
            Err(SaveError::Changed) => {
                f.external = true;
                f.force = true;
                self.status = t!("Otro equipo o programa cambió esta conexión. Pulsa Guardar otra vez para sobrescribirla.").into();
            }
            Err(SaveError::Io(e)) => self.status = tf!("No se pudo guardar: {e}", e = e),
        }
    }

    fn delete(&mut self) {
        let Some(id) = self.selected.clone() else { return };
        if self.form.as_ref().is_some_and(|f| f.mtime.is_some()) {
            if let Err(e) = self.catalog.delete(&id) {
                self.status = tf!("No se pudo borrar: {e}", e = e);
                return;
            }
            let key = Secrets::key(&self.catalog.dir, &id);
            let _ = self.secrets.set(key, None);
        }
        self.form = None;
        self.selected = None;
        self.reload();
        self.status = t!("Conexión borrada.").into();
    }

    fn request_fields(&self, f: &Form) -> Result<Vec<(String, String)>, String> {
        bridge::conn_fields(&f.built(), &f.password, &self.catalog.dir)
    }

    fn send(&mut self, cmd: &str, fields: &[(String, String)], kind: Pending) -> u64 {
        let refs: Vec<(&str, &str)> = fields.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let id = self.bridge.send(cmd, &refs);
        self.pending.insert(id, kind);
        id
    }

    fn test(&mut self) {
        let Some(f) = &self.form else { return };
        self.test_name = f.conn.name.clone();
        let fields = match self.request_fields(f) {
            Ok(x) => x,
            Err(e) => {
                self.test = TestState::Err(ErrInfo::plain(e));
                return;
            }
        };
        // Margen sobre el timeout del driver: la primera vez Java arranca.
        let limit = Duration::from_secs(f.conn.timeout.max(1) as u64 + if self.java.is_some() { 5 } else { 20 });
        let id = self.send("probar", &fields, Pending::Test);
        self.test = TestState::Running { id, since: Instant::now(), limit };
    }

    fn connect(&mut self) {
        let Some(f) = &self.form else { return };
        let fields = match self.request_fields(f) {
            Ok(x) => x,
            Err(e) => {
                self.console.error = Some(ErrInfo::plain(e));
                self.console.active = None;
                self.console.queue.clear();
                return;
            }
        };
        let name = f.conn.name.clone();
        let conn_id = f.conn.id.clone();
        self.disconnect();
        let id = self.send("abrir", &fields, Pending::Open);
        self.console.opening = Some(id);
        self.console.session = Some((String::new(), name));
        self.console.session_conn = conn_id;
        self.console.error = None;
    }

    fn disconnect(&mut self) {
        if let Some((s, _)) = self.console.session.take().filter(|(s, _)| !s.is_empty()) {
            self.send("cerrar", &[("sesion".into(), s)], Pending::Other);
        }
        self.console.opening = None;
        self.console.queued = false;
        self.console.queue.clear();
        self.explorer = Explorer::default();
    }

    fn session_id(&self) -> Option<String> {
        self.console.session.as_ref().filter(|(s, _)| !s.is_empty()).map(|(s, _)| s.clone())
    }

    /// Ejecuta la selección o la sentencia bajo el cursor (`all` = todo el script).
    fn run_sql(&mut self, all: bool) {
        if self.console.running.is_some() {
            return;
        }
        let text = self.console.sql.clone();
        let piece = match (&self.console.selection, all) {
            (Some(r), false) if r.end <= text.len() && !text[r.clone()].trim().is_empty() => text[r.clone()].to_owned(),
            (_, false) => match crate::sql::at_cursor(&text, self.console.cursor.min(text.len())) {
                Some(r) => text[r].to_owned(),
                None => return,
            },
            (_, true) => text,
        };
        let stmts: Vec<String> = crate::sql::split(&piece).into_iter().map(|r| crate::sql::for_jdbc(&piece[r])).filter(|s| !s.is_empty()).collect();
        if stmts.is_empty() {
            return;
        }
        let c = &mut self.console;
        c.tabs.clear();
        c.active = None;
        c.log.clear();
        c.error = None;
        c.queue = stmts.into();
        self.next_statement();
    }

    /// Lanza la siguiente sentencia de la cola (abre la sesión si hace falta).
    fn next_statement(&mut self) {
        let Some(s) = self.session_id() else {
            if self.console.opening.is_none() {
                if self.form.is_none() {
                    self.status = t!("Elige una conexión.").into();
                    self.console.queue.clear();
                    return;
                }
                self.connect_keep_queue();
            }
            self.console.queued = true;
            return;
        };
        let Some(sql) = self.console.queue.pop_front() else { return };
        self.history_add(&sql);
        let fields = [("sesion".to_owned(), s), ("sql".to_owned(), sql.clone()), ("max".to_owned(), self.settings.max_rows.to_string())];
        let id = self.send("consulta", &fields, Pending::Query);
        let c = &mut self.console;
        c.running = Some((id, Instant::now()));
        c.current = sql;
        c.current_tab = false;
    }

    /// Como `connect` pero sin vaciar la cola de sentencias.
    fn connect_keep_queue(&mut self) {
        let queue = std::mem::take(&mut self.console.queue);
        self.connect();
        self.console.queue = queue;
    }

    fn cancel_sql(&mut self) {
        self.console.queue.clear();
        if let Some((id, _)) = self.console.running {
            self.send("cancelar", &[("objetivo".into(), id.to_string())], Pending::Other);
        }
    }

    // ---------- historial de SQL (por conexión, en este equipo) ----------

    fn history_path(&self, conn: &str) -> Option<std::path::PathBuf> {
        Some(store::config_dir()?.join("historial").join(format!("{}.txt", store::slug(conn))))
    }

    /// Entradas `(segundos unix, sql)`, la más reciente al final.
    fn history(&self, conn: &str) -> Vec<(u64, String)> {
        let Some(text) = self.history_path(conn).and_then(|p| std::fs::read_to_string(p).ok()) else { return Vec::new() };
        text.lines()
            .filter_map(|l| {
                let (t, q) = l.split_once('\t')?;
                // Mismo escape que el protocolo (\\ \t \n \r).
                Some((t.parse().ok()?, crate::proto::split(q).into_iter().next()??.into()))
            })
            .collect()
    }

    fn history_add(&mut self, sql: &str) {
        let conn = self.console.session_conn.clone();
        if conn.is_empty() || std::env::var_os("CONECTOR_CAPTURA").is_some() {
            return;
        }
        let Some(path) = self.history_path(&conn) else { return };
        let mut h = self.history(&conn);
        h.retain(|(_, q)| q != sql);
        let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        h.push((now, sql.to_owned()));
        let skip = h.len().saturating_sub(500);
        let mut text = String::new();
        for (t, q) in &h[skip..] {
            text.push_str(&format!("{t}\t"));
            crate::proto::escape_into(&mut text, q);
            text.push('\n');
        }
        if let Some(d) = path.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let _ = store::write_atomic(&path, text.as_bytes(), true);
    }

    fn meta(&mut self, req: Meta) {
        let Some(s) = self.session_id() else { return };
        let mut f = vec![("sesion".to_owned(), s)];
        match &req {
            Meta::Schemas => {
                f.push(("nivel".into(), "esquemas".into()));
                self.explorer.loading = true;
                self.explorer.error = None;
            }
            Meta::Tables(cat, sch) => {
                f.extend([("nivel".into(), "tablas".into()), ("catalogo".into(), cat.clone()), ("esquema".into(), sch.clone())]);
                if let Some(x) = self.explorer.schemas.iter_mut().find(|x| &x.cat == cat && &x.sch == sch) {
                    x.loading = true;
                }
            }
            Meta::Columns(label) | Meta::Ddl(label, _) => {
                let (q, table) = label.rsplit_once('.').unwrap_or(("", label));
                let sch = self.explorer.schemas.iter().find(|x| x.qualifier() == q);
                let (cat, sch) = sch.map(|x| (x.cat.clone(), x.sch.clone())).unwrap_or_default();
                let level = if matches!(req, Meta::Ddl(..)) { "ddl" } else { "columnas" };
                f.extend([("nivel".into(), level.into()), ("catalogo".into(), cat), ("esquema".into(), sch), ("tabla".into(), table.to_owned())]);
                if let Meta::Ddl(_, kind) = &req {
                    f.push(("tipo".into(), kind.clone()));
                }
            }
        }
        let id = self.send("meta", &f, Pending::Meta(req));
        self.meta_buf.insert(id, (Vec::new(), Vec::new()));
    }

    fn restart_java(&mut self) {
        self.bridge.restart();
        self.status = t!("Java reiniciado: se cortan conexiones y consultas en curso.").into();
    }

    // ---------- frase común ----------

    fn unlock(&mut self) {
        let Some(lock) = &self.lock else { return };
        match lock.unlock(&self.pass_edit.unlock) {
            Some(k) => {
                self.key = Some(k);
                if self.pass_edit.remember {
                    let _ = self.secrets.set_catalog_key(&self.catalog.dir, Some(&k));
                }
                self.pass_edit = PassEdit { remember: self.pass_edit.remember, ..Default::default() };
                self.status = t!("Catálogo desbloqueado.").into();
                if let Some(id) = self.selected.clone().filter(|_| !self.form.as_ref().is_some_and(|f| f.dirty())) {
                    self.select(id);
                }
            }
            None => self.status = t!("La frase no es correcta.").into(),
        }
    }

    /// Crea la frase o la cambia (vuelve a cifrar todas las contraseñas compartidas).
    fn set_passphrase(&mut self) {
        let p = &self.pass_edit;
        if p.new1 != p.new2 {
            self.status = t!("Las dos frases no coinciden.").into();
            return;
        }
        if self.lock.is_some() && self.key.is_none() {
            self.status = t!("Primero desbloquea el catálogo con la frase actual.").into();
            return;
        }
        let (lock, key) = match Lock::create(&p.new1) {
            Ok(x) => x,
            Err(e) => {
                self.status = e;
                return;
            }
        };
        // Volver a cifrar con la nueva clave.
        let mut redo = Vec::new();
        for e in self.catalog.load() {
            if e.conn.secret.is_empty() {
                continue;
            }
            match self.key.as_ref().and_then(|k| crypto::decrypt(k, &e.conn.secret)) {
                Some(pass) => redo.push((e.conn, pass)),
                None => {
                    self.status = tf!("No se pudo descifrar «{nombre}»: no se cambió nada.", nombre = e.conn.name);
                    return;
                }
            }
        }
        if let Err(e) = lock.save(&self.catalog.dir) {
            self.status = tf!("No se pudo guardar: {e}", e = e);
            return;
        }
        let n = redo.len();
        for (mut c, pass) in redo {
            c.secret = crypto::encrypt(&key, &pass);
            let _ = self.catalog.save(&c, None);
        }
        self.lock = Some(lock);
        self.key = Some(key);
        let _ = self.secrets.set_catalog_key(&self.catalog.dir, if self.pass_edit.remember { Some(&key) } else { None });
        self.pass_edit = PassEdit { remember: self.pass_edit.remember, ..Default::default() };
        self.status = tf!("Frase común guardada; {n} contraseñas cifradas de nuevo. Compártela con tu equipo por un canal seguro.", n = n);
        self.reload();
        if let Some(id) = self.selected.clone() {
            self.select(id);
        }
    }

    fn forget_key(&mut self) {
        self.key = None;
        let _ = self.secrets.set_catalog_key(&self.catalog.dir, None);
        if let Some(id) = self.selected.clone() {
            self.select(id);
        }
        self.status = t!("Este equipo ya no recuerda la frase.").into();
    }

    // ---------- eventos de Java ----------

    fn poll_bridge(&mut self) {
        while let Ok(ev) = self.bridge.rx.try_recv() {
            match ev {
                Event::Msg(m) if m.id == 0 => {
                    self.java = Some(format!("Java {} ({})", m.get("java").unwrap_or("?"), m.get("vm").unwrap_or("?")));
                }
                Event::Msg(m) => self.on_msg(m),
                Event::Failed(id, e) => self.fail(id, ErrInfo::plain(e)),
                Event::Exited(log) => {
                    self.java = None;
                    self.java_log = log.clone();
                    let detail = if log.is_empty() { String::new() } else { format!("{}\n{log}", t!("Últimas líneas de Java:")) };
                    let ids: Vec<u64> = self.pending.keys().copied().collect();
                    for id in ids {
                        self.fail(id, ErrInfo { msg: t!("Java se cerró").into(), detail: detail.clone() });
                    }
                    if self.console.session.take().is_some() {
                        self.console.info = t!("Sesión cerrada (Java terminó).").into();
                    }
                    self.console.opening = None;
                    self.explorer = Explorer::default();
                }
            }
        }
    }

    fn fail(&mut self, id: u64, e: ErrInfo) {
        self.meta_buf.remove(&id);
        match self.pending.remove(&id) {
            Some(Pending::Test) => {
                if matches!(self.test, TestState::Running { id: t, .. } if t == id) {
                    self.test = TestState::Err(e);
                }
            }
            Some(Pending::Open) => {
                if self.console.opening == Some(id) {
                    self.console.opening = None;
                    self.console.session = None;
                    self.console.queued = false;
                    self.console.error = Some(e);
                }
            }
            Some(Pending::Query) => {
                let c = &mut self.console;
                if c.running.is_some_and(|(r, _)| r == id) {
                    c.running = None;
                    c.log.push((false, format!("✖ {} — {}", short_title(&c.current), e.msg)));
                    c.error = Some(e);
                    c.active = None;
                    if c.keep_going && !c.queue.is_empty() {
                        self.next_statement();
                    } else {
                        c.queue.clear();
                    }
                }
            }
            Some(Pending::Meta(req)) => match req {
                Meta::Schemas => {
                    self.explorer.loading = false;
                    self.explorer.error = Some(e.msg);
                }
                Meta::Tables(cat, sch) => {
                    if let Some(x) = self.explorer.schemas.iter_mut().find(|x| x.cat == cat && x.sch == sch) {
                        x.loading = false;
                    }
                    self.status = e.msg;
                }
                Meta::Columns(_) | Meta::Ddl(..) => {
                    self.console.error = Some(e);
                    self.console.active = None;
                }
            },
            Some(Pending::Export) => {
                if let Some(j) = self.export_job.take() {
                    if let Some(x) = j.exporter {
                        x.abort();
                    }
                }
                self.status = tf!("No se pudo exportar: {e}", e = e.msg);
            }
            Some(Pending::Import) => {
                if let Some(j) = self.import_job.take() {
                    let at = j.done_rows + 1;
                    self.status = tf!("Importación detenida en el lote que empieza en la fila {n} (las anteriores quedaron guardadas): {e}", n = at, e = e.msg);
                    if let Some(d) = &mut self.import_dialog {
                        d.error = Some(self.status.clone());
                    }
                }
            }
            Some(Pending::Other) => self.status = e.msg,
            None => {}
        }
    }

    fn on_msg(&mut self, m: Msg) {
        if &*m.kind == "error" {
            return self.fail(m.id, ErrInfo::from_msg(&m));
        }
        match self.pending.get(&m.id) {
            Some(Pending::Test) => {
                self.pending.remove(&m.id);
                if matches!(self.test, TestState::Running { id, .. } if id == m.id) {
                    self.test = TestState::Ok(m.pairs().map(|(k, v)| (k.to_owned(), v.to_owned())).collect());
                }
            }
            Some(Pending::Open) => {
                self.pending.remove(&m.id);
                if self.console.opening != Some(m.id) {
                    // Ya no se quiere: cerrarla.
                    if let Some(s) = m.get("sesion") {
                        let s = s.to_owned();
                        self.send("cerrar", &[("sesion".into(), s)], Pending::Other);
                    }
                    return;
                }
                self.console.opening = None;
                if let (Some(s), Some(sess)) = (m.get("sesion"), &mut self.console.session) {
                    sess.0 = s.to_owned();
                }
                self.console.info = tf!(
                    "Conectado en {ms} ms · {producto} {version}",
                    ms = m.get("ms").unwrap_or("?"),
                    producto = m.get("producto").unwrap_or(""),
                    version = m.get("version").unwrap_or("")
                );
                self.meta(Meta::Schemas);
                if std::mem::take(&mut self.console.queued) {
                    self.next_statement();
                }
            }
            Some(Pending::Query) => {
                let c = &mut self.console;
                if c.running.is_none_or(|(r, _)| r != m.id) {
                    if &*m.kind == "fin" {
                        self.pending.remove(&m.id);
                    }
                    return;
                }
                match &*m.kind {
                    "columnas" => {
                        let title = short_title(&c.current);
                        let sql = c.current.clone();
                        c.push_tab(ResultTab { title, sql, cols: split_cols(&m), ..Default::default() });
                        c.current_tab = true;
                    }
                    "fila" => {
                        if let Some(t) = c.tabs.last_mut().filter(|_| c.current_tab) {
                            t.rows.push(m.fields.into_boxed_slice());
                        }
                    }
                    "fin" => {
                        self.pending.remove(&m.id);
                        let ms = m.get("ms").unwrap_or("?");
                        let upd = m.get("actualizadas").unwrap_or("");
                        let title = short_title(&c.current);
                        if c.current_tab {
                            let t = c.tabs.last_mut().unwrap();
                            t.info = if m.get("mas") == Some("true") {
                                tf!("{n} filas (hay más: sube el límite en Ajustes) · {ms} ms", n = t.rows.len(), ms = ms)
                            } else {
                                tf!("{n} filas · {ms} ms", n = t.rows.len(), ms = ms)
                            };
                            t.rows.shrink_to_fit();
                            let line = format!("✔ {title} — {}", t.info);
                            c.log.push((true, line));
                        } else {
                            let info = if upd.is_empty() { tf!("Hecho en {ms} ms", ms = ms) } else { tf!("Filas afectadas: {n} · {ms} ms", n = upd, ms = ms) };
                            c.log.push((true, format!("✔ {title} — {info}")));
                            if c.tabs.is_empty() {
                                c.active = None;
                            }
                        }
                        c.running = None;
                        if !c.queue.is_empty() {
                            self.next_statement();
                        }
                    }
                    _ => {}
                }
            }
            Some(Pending::Meta(_)) => {
                let Some(buf) = self.meta_buf.get_mut(&m.id) else { return };
                match &*m.kind {
                    "columnas" => buf.0 = split_cols(&m).into_iter().map(|c| c.0).collect(),
                    "fila" => buf.1.push(m.fields.into_boxed_slice()),
                    "fin" => {
                        let (cols, rows) = self.meta_buf.remove(&m.id).unwrap_or_default();
                        if let Some(Pending::Meta(req)) = self.pending.remove(&m.id) {
                            self.meta_done(req, cols, rows);
                        }
                    }
                    _ => {}
                }
            }
            Some(Pending::Export) => self.export_msg(m),
            Some(Pending::Import) => {
                self.pending.remove(&m.id);
                if let Some(j) = &mut self.import_job {
                    if let Some((_, n)) = j.inflight.take() {
                        j.done_rows += n as u64;
                    }
                }
                self.import_next();
            }
            Some(Pending::Other) => {
                self.pending.remove(&m.id);
            }
            None => {}
        }
    }

    // ---------- exportar ----------

    fn open_export(&mut self) {
        let Some(i) = self.console.active else { return };
        let Some(t) = self.console.tabs.get(i) else { return };
        let fmt = self.export_dialog.as_ref().map_or(crate::export::Format::Csv, |d| d.fmt);
        let base = store::slug(&if t.sql.is_empty() { t.title.clone() } else { guess_table(&t.sql) });
        let dir = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(std::path::PathBuf::from).unwrap_or_default();
        self.export_dialog = Some(ExportDialog {
            fmt,
            all: !t.sql.is_empty() && self.session_id().is_some(),
            table: guess_table(&t.sql),
            path: dir.join(format!("{base}.{}", fmt.ext())).display().to_string(),
            tab: i,
        });
    }

    fn start_export(&mut self) {
        let Some(d) = self.export_dialog.take() else { return };
        let path = std::path::PathBuf::from(d.path.trim());
        let Some(t) = self.console.tabs.get(d.tab) else { return };
        let mut x = match crate::export::Exporter::create(&path, d.fmt, &d.table) {
            Ok(x) => x,
            Err(e) => {
                self.status = tf!("No se pudo exportar: {e}", e = e);
                return;
            }
        };
        if !d.all || t.sql.is_empty() {
            // Solo lo cargado: se escribe ya.
            let r = x.columns(&t.cols).and_then(|_| t.rows.iter().try_for_each(|r| x.row(r))).and_then(|_| x.finish());
            self.status = match r {
                Ok(n) => tf!("Exportadas {n} filas a {ruta}", n = n, ruta = path.display()),
                Err(e) => tf!("No se pudo exportar: {e}", e = e),
            };
            return;
        }
        let Some(s) = self.session_id() else {
            x.abort();
            return;
        };
        let fields = [("sesion".to_owned(), s), ("sql".to_owned(), t.sql.clone()), ("max".to_owned(), "0".to_owned())];
        let id = self.send("consulta", &fields, Pending::Export);
        self.export_job = Some(ExportJob { id, exporter: Some(x), path, started: Instant::now() });
    }

    fn export_msg(&mut self, m: Msg) {
        let Some(j) = &mut self.export_job else {
            self.pending.remove(&m.id);
            return;
        };
        let Some(x) = j.exporter.as_mut() else { return };
        let r = match &*m.kind {
            "columnas" => x.columns(&split_cols(&m)),
            "fila" => x.row(&m.fields),
            "fin" => {
                self.pending.remove(&m.id);
                let j = self.export_job.take().unwrap();
                let n = j.exporter.map(|x| x.finish());
                self.status = match n {
                    Some(Ok(n)) if n >= crate::export::XLSX_MAX_ROWS && j.path.extension().is_some_and(|e| e == "xlsx") => {
                        tf!("Exportadas {n} filas a {ruta} (límite de Excel: el resto no cabe; usa CSV)", n = n, ruta = j.path.display())
                    }
                    Some(Ok(n)) => tf!("Exportadas {n} filas a {ruta} en {s} s", n = n, ruta = j.path.display(), s = format!("{:.1}", j.started.elapsed().as_secs_f32())),
                    Some(Err(e)) => tf!("No se pudo exportar: {e}", e = e),
                    None => String::new(),
                };
                return;
            }
            _ => Ok(()),
        };
        if let Err(e) = r {
            // Disco lleno o similar: cancelar la consulta y dejarlo.
            let id = j.id;
            if let Some(x) = self.export_job.take().and_then(|j| j.exporter) {
                x.abort();
            }
            self.send("cancelar", &[("objetivo".into(), id.to_string())], Pending::Other);
            self.status = tf!("No se pudo exportar: {e}", e = e);
        }
    }

    fn cancel_export(&mut self) {
        if let Some(j) = self.export_job.take() {
            self.pending.remove(&j.id);
            self.send("cancelar", &[("objetivo".into(), j.id.to_string())], Pending::Other);
            if let Some(x) = j.exporter {
                x.abort();
            }
            self.status = t!("Exportación cancelada.").into();
        }
    }

    // ---------- importar ----------

    fn open_import(&mut self, table: &str) {
        let d = self.import_dialog.get_or_insert_with(|| ImportDialog { header: true, empty_null: true, sep: b',', ..Default::default() });
        if !table.is_empty() {
            d.table = table.to_owned();
        }
    }

    /// Lee las primeras filas del CSV elegido para la vista previa.
    fn import_preview(d: &mut ImportDialog, guess: bool) {
        d.preview.clear();
        d.error = None;
        let file = match std::fs::File::open(d.path.trim()) {
            Ok(f) => f,
            Err(e) => {
                d.error = Some(e.to_string());
                return;
            }
        };
        if guess {
            let first = std::io::BufRead::lines(std::io::BufReader::new(&file)).next().and_then(Result::ok).unwrap_or_default();
            d.sep = crate::export::guess_sep(&first);
        }
        let Ok(file) = std::fs::File::open(d.path.trim()) else { return };
        let mut r = crate::export::CsvReader::new(file, d.sep);
        while d.preview.len() < 6 {
            match r.record() {
                Ok(Some(rec)) => d.preview.push(rec),
                _ => break,
            }
        }
        if d.header {
            if let Some(h) = d.preview.first() {
                d.cols = h.join(", ");
            }
        }
    }

    fn start_import(&mut self) {
        let Some(d) = &self.import_dialog else { return };
        if self.session_id().is_none() {
            self.status = t!("Conecta primero (consola SQL).").into();
            return;
        }
        let cols: Vec<String> = d.cols.split(',').map(|c| c.trim().to_owned()).filter(|c| !c.is_empty()).collect();
        if d.table.trim().is_empty() || cols.is_empty() {
            self.status = t!("Indica la tabla y las columnas.").into();
            return;
        }
        let file = match std::fs::File::open(d.path.trim()) {
            Ok(f) => f,
            Err(e) => {
                self.status = e.to_string();
                return;
            }
        };
        let mut reader = crate::export::CsvReader::new(file, d.sep);
        if d.header {
            let _ = reader.record();
        }
        self.import_job = Some(ImportJob { reader, table: d.table.trim().to_owned(), cols, empty_null: d.empty_null, inflight: None, done_rows: 0, started: Instant::now(), eof: false });
        if let Some(d) = &mut self.import_dialog {
            d.error = None;
        }
        self.import_next();
    }

    /// Envía el siguiente lote o termina.
    fn import_next(&mut self) {
        let Some(s) = self.session_id() else {
            self.import_job = None;
            return;
        };
        let Some(j) = &mut self.import_job else { return };
        if j.inflight.is_some() {
            return;
        }
        let mut fields: Vec<(String, String)> = vec![("sesion".into(), s), ("tabla".into(), j.table.clone()), ("columnas".into(), j.cols.join("\u{1}"))];
        let mut n = 0;
        while n < IMPORT_BATCH && !j.eof {
            match j.reader.record() {
                Ok(Some(rec)) => {
                    if rec.len() == 1 && rec[0].is_empty() {
                        continue; // línea vacía
                    }
                    for i in 0..j.cols.len() {
                        match rec.get(i) {
                            Some(v) if !(j.empty_null && v.is_empty()) => fields.push(("v".into(), v.clone())),
                            _ => fields.push(("z".into(), String::new())),
                        }
                    }
                    n += 1;
                }
                Ok(None) => j.eof = true,
                Err(e) => {
                    self.status = e.to_string();
                    self.import_job = None;
                    return;
                }
            }
        }
        if n == 0 {
            let j = self.import_job.take().unwrap();
            self.status = tf!("Importadas {n} filas en {tabla} en {s} s", n = j.done_rows, tabla = j.table, s = format!("{:.1}", j.started.elapsed().as_secs_f32()));
            if let Some(d) = &mut self.import_dialog {
                d.error = None;
            }
            self.import_dialog = None;
            return;
        }
        let id = self.send("lote", &fields, Pending::Import);
        if let Some(j) = &mut self.import_job {
            j.inflight = Some((id, n));
        }
    }

    fn export_window(&mut self, ctx: &Context) {
        let Some(d) = &mut self.export_dialog else { return };
        let mut open = true;
        let mut go = false;
        let mut pick = false;
        let has_sql = self.console.tabs.get(d.tab).is_some_and(|t| !t.sql.is_empty());
        let loaded = self.console.tabs.get(d.tab).map_or(0, |t| t.rows.len());
        let connected = self.console.session.as_ref().is_some_and(|(s, _)| !s.is_empty());
        Window::new(t!("Exportar")).id(win_id(Win::Export)).open(&mut open).collapsible(false).resizable(false).show(ctx, |ui| {
            Grid::new("exportar").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label(t!("Formato"));
                ui.horizontal(|ui| {
                    for f in crate::export::Format::ALL {
                        if ui.radio_value(&mut d.fmt, f, f.label()).changed() {
                            d.path = std::path::Path::new(&d.path).with_extension(f.ext()).display().to_string();
                        }
                    }
                });
                ui.end_row();
                ui.label(t!("Filas"));
                ui.vertical(|ui| {
                    ui.radio_value(&mut d.all, false, tf!("Solo las cargadas ({n})", n = loaded));
                    ui.add_enabled_ui(has_sql && connected, |ui| {
                        ui.radio_value(&mut d.all, true, t!("Todas: vuelve a ejecutar la consulta sin límite y escribe al vuelo"));
                    });
                });
                ui.end_row();
                if d.fmt == crate::export::Format::Insert {
                    ui.label(t!("Tabla"));
                    ui.add(TextEdit::singleline(&mut d.table).desired_width(260.0));
                    ui.end_row();
                }
                ui.label(t!("Archivo"));
                ui.horizontal(|ui| {
                    ui.add(TextEdit::singleline(&mut d.path).desired_width(320.0));
                    if ui.button(t!("Elegir…")).clicked() {
                        pick = true;
                    }
                });
                ui.end_row();
            });
            if ui.button(t!("Exportar")).clicked() {
                go = true;
            }
        });
        if pick {
            let tx = self.dialog.0.clone();
            let ctx2 = ctx.clone();
            let p = std::path::PathBuf::from(&d.path);
            std::thread::spawn(move || {
                let mut dlg = rfd::FileDialog::new();
                if let Some(dir) = p.parent() {
                    dlg = dlg.set_directory(dir);
                }
                if let Some(n) = p.file_name() {
                    dlg = dlg.set_file_name(n.to_string_lossy());
                }
                if let Some(f) = dlg.save_file() {
                    let _ = tx.send(Picked::ExportPath(f));
                    ctx2.request_repaint();
                }
            });
        }
        if go {
            self.start_export();
        } else if !open {
            self.export_dialog = None;
        }
    }

    fn import_window(&mut self, ctx: &Context) {
        let Some(d) = &mut self.import_dialog else { return };
        let mut open = true;
        let mut go = false;
        let mut pick = false;
        let mut reload = false;
        let mut new_file = false;
        let busy = self.import_job.is_some();
        Window::new(t!("Importar CSV")).id(win_id(Win::Import)).open(&mut open).collapsible(false).default_width(560.0).show(ctx, |ui| {
            Grid::new("importar").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label(t!("Archivo"));
                ui.horizontal(|ui| {
                    let r = ui.add(TextEdit::singleline(&mut d.path).desired_width(320.0));
                    new_file |= r.lost_focus();
                    if ui.button(t!("Elegir…")).clicked() {
                        pick = true;
                    }
                });
                ui.end_row();
                ui.label(t!("Separador"));
                ui.horizontal(|ui| {
                    for (c, l) in [(b',', ","), (b';', ";"), (b'\t', "Tab"), (b'|', "|")] {
                        reload |= ui.radio_value(&mut d.sep, c, l).changed();
                    }
                    reload |= ui.checkbox(&mut d.header, t!("Primera fila = nombres de columna")).changed();
                    ui.checkbox(&mut d.empty_null, t!("Vacío = NULL"));
                });
                ui.end_row();
                ui.label(t!("Tabla"));
                ui.add(TextEdit::singleline(&mut d.table).hint_text(t!("esquema.tabla (ya existente)")).desired_width(320.0));
                ui.end_row();
                ui.label(t!("Columnas"));
                ui.add(TextEdit::singleline(&mut d.cols).hint_text(t!("en el orden del CSV, separadas por comas")).desired_width(420.0));
                ui.end_row();
            });
            if !d.preview.is_empty() {
                ui.separator();
                ui.weak(t!("Vista previa"));
                ScrollArea::horizontal().id_salt("vista_csv").show(ui, |ui| {
                    Grid::new("vista_previa").striped(true).show(ui, |ui| {
                        for (i, rec) in d.preview.iter().enumerate() {
                            for v in rec {
                                let t = RichText::new(short_title(v)).monospace();
                                ui.label(if i == 0 && d.header { t.strong() } else { t });
                            }
                            ui.end_row();
                        }
                    });
                });
            }
            if let Some(e) = &d.error {
                ui.colored_label(RED, e);
            }
            ui.horizontal(|ui| {
                if ui.add_enabled(!busy, Button::new(t!("Importar"))).clicked() {
                    go = true;
                }
                ui.weak(t!("Lotes de 500 filas, cada uno en una transacción."));
            });
        });
        if new_file || reload {
            Self::import_preview(d, new_file);
        }
        if pick {
            let tx = self.dialog.0.clone();
            let ctx2 = ctx.clone();
            std::thread::spawn(move || {
                if let Some(f) = rfd::FileDialog::new().add_filter("CSV", &["csv", "txt", "tsv"]).pick_file() {
                    let _ = tx.send(Picked::ImportPath(f));
                    ctx2.request_repaint();
                }
            });
        }
        if go {
            self.start_import();
        } else if !open && !busy {
            self.import_dialog = None;
        }
    }

    fn meta_done(&mut self, req: Meta, cols: Vec<String>, rows: Vec<Box<[Option<Box<str>>]>>) {
        let idx = |name: &str| cols.iter().position(|c| c.eq_ignore_ascii_case(name));
        let get = |r: &[Option<Box<str>>], i: Option<usize>| i.and_then(|i| r.get(i)).and_then(|v| v.as_deref()).unwrap_or("").to_owned();
        match req {
            Meta::Schemas => {
                let (c, s) = (idx("TABLE_CAT"), idx("TABLE_SCHEM"));
                self.explorer.schemas = rows.iter().map(|r| Schema { cat: get(r, c), sch: get(r, s), tables: None, loading: false, open: false }).collect();
                self.explorer.loading = false;
            }
            Meta::Tables(cat, sch) => {
                let (n, k) = (idx("TABLE_NAME"), idx("TABLE_TYPE"));
                let mut tables: Vec<(String, String)> = rows.iter().map(|r| (get(r, n), get(r, k))).collect();
                tables.sort();
                if let Some(x) = self.explorer.schemas.iter_mut().find(|x| x.cat == cat && x.sch == sch) {
                    x.tables = Some(tables);
                    x.loading = false;
                }
            }
            Meta::Columns(label) => {
                // Resumen legible de getColumns en la tabla de resultados.
                let pick = [
                    ("COLUMN_NAME", t!("columna")),
                    ("TYPE_NAME", t!("tipo")),
                    ("COLUMN_SIZE", t!("tamaño")),
                    ("DECIMAL_DIGITS", t!("decimales")),
                    ("IS_NULLABLE", t!("nulos")),
                    ("COLUMN_DEF", t!("por defecto")),
                    ("REMARKS", t!("comentario")),
                ];
                let found: Vec<(usize, &str)> = pick.iter().filter_map(|(c, l)| Some((idx(c)?, *l))).collect();
                let rows: Vec<_> = rows.iter().map(|r| found.iter().map(|(i, _)| r.get(*i).cloned().flatten()).collect::<Vec<_>>().into_boxed_slice()).collect();
                let info = tf!("Columnas de {tabla}: {n}", tabla = label, n = rows.len());
                let cols = found.iter().map(|(_, l)| ((*l).to_owned(), String::new())).collect();
                self.console.push_tab(ResultTab { title: format!("▦ {label}"), sql: String::new(), cols, rows, widths: Vec::new(), info, text: None });
            }
            Meta::Ddl(label, _) => {
                let ddl = rows.first().and_then(|r| r.first().cloned().flatten()).map(String::from).unwrap_or_default();
                // Si ya hay una pestaña con la DDL de esa tabla, se reemplaza.
                let title = format!("DDL · {label}");
                self.console.tabs.retain(|t| !(t.text.is_some() && t.title == title));
                self.console.push_tab(ResultTab { title, info: tf!("DDL de {tabla}", tabla = label), text: Some(ddl), ..Default::default() });
            }
        }
    }

    fn check_timeouts(&mut self, ctx: &Context) {
        if let TestState::Running { id, since, limit } = self.test {
            if since.elapsed() > limit {
                self.pending.remove(&id);
                self.test = TestState::Err(ErrInfo::plain(tf!(
                    "Sin respuesta tras {s} s. El driver puede seguir intentándolo; «Reiniciar Java» lo corta.",
                    s = limit.as_secs()
                )));
            } else {
                ctx.request_repaint_after(Duration::from_millis(200));
            }
        }
        if self.console.running.is_some() || self.console.opening.is_some() || self.export_job.is_some() || self.import_job.is_some() {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        if self.last_scan.elapsed() >= SCAN_EVERY {
            self.last_scan = Instant::now();
            if self.catalog.signature() != self.signature || Lock::load(&self.catalog.dir) != self.lock {
                self.reload();
            }
        }
        ctx.request_repaint_after(SCAN_EVERY);
        while let Ok(p) = self.dialog.1.try_recv() {
            match p {
                Picked::ExportPath(f) => {
                    if let Some(d) = &mut self.export_dialog {
                        d.path = f.display().to_string();
                    }
                }
                Picked::ImportPath(f) => {
                    if let Some(d) = &mut self.import_dialog {
                        d.path = f.display().to_string();
                        Self::import_preview(d, true);
                    }
                }
                Picked::Catalog(dir) => self.settings_edit.catalog = dir.display().to_string(),
                Picked::Jar(files) => {
                    if let Some(f) = &mut self.form {
                        for file in files {
                            let rel = file.strip_prefix(&self.catalog.dir).map(|r| r.to_path_buf()).unwrap_or(file);
                            if !f.jars.is_empty() && !f.jars.ends_with('\n') {
                                f.jars.push('\n');
                            }
                            f.jars.push_str(&rel.display().to_string());
                        }
                    }
                }
            }
        }
        // Descargas terminadas.
        let java_done = self.java_dl.as_ref().map(|s| s.lock().unwrap().clone());
        if let Some(Dl::Done(path)) = java_done {
            self.java_dl = None;
            self.java_ok = true;
            self.bridge = new_bridge(ctx, &self.settings);
            self.java = None;
            self.pending.clear();
            self.status = tf!("Java portátil listo: {ruta}", ruta = path);
            self.start_server();
        }
        let driver_done = self.driver_dl.as_ref().map(|(s, add)| (s.lock().unwrap().clone(), *add));
        if let Some((Dl::Done(rel), add)) = driver_done {
            self.driver_dl = None;
            if let Some(f) = self.form.as_mut().filter(|_| add) {
                if !f.jars.lines().any(|l| l.trim() == rel) {
                    if !f.jars.is_empty() && !f.jars.ends_with('\n') {
                        f.jars.push('\n');
                    }
                    f.jars.push_str(&rel);
                }
            }
            self.status = tf!("Driver descargado: {ruta}", ruta = rel);
        }
    }

    fn pick(&self, ctx: &Context, folder: bool) {
        let tx = self.dialog.0.clone();
        let ctx = ctx.clone();
        let start = self.catalog.dir.clone();
        std::thread::spawn(move || {
            let d = rfd::FileDialog::new().set_directory(&start);
            let r = if folder {
                d.pick_folder().map(Picked::Catalog)
            } else {
                d.add_filter("Driver JDBC", &["jar"]).pick_files().map(Picked::Jar)
            };
            if let Some(r) = r {
                let _ = tx.send(r);
                ctx.request_repaint();
            }
        });
    }

    fn waker(ctx: &Context) -> Arc<dyn Fn() + Send + Sync> {
        let ctx = ctx.clone();
        Arc::new(move || ctx.request_repaint())
    }

    fn download_java(&mut self, ctx: &Context) {
        let st: javadl::Shared = Arc::new(Mutex::new(Dl::Running(String::new())));
        javadl::download_java(&st, Self::waker(ctx));
        self.java_dl = Some(st);
    }

    fn download_driver(&mut self, ctx: &Context, template: &str) {
        let st: javadl::Shared = Arc::new(Mutex::new(Dl::Running(String::new())));
        // Si la conexión lista sus jar, el nuevo se añade; si no, ya lo coge de drivers/.
        let add = self.form.as_ref().is_some_and(|f| !f.jars.trim().is_empty());
        javadl::download_driver(&st, Self::waker(ctx), template.to_owned(), self.catalog.dir.clone());
        self.driver_dl = Some((st, add));
    }

    fn apply_settings(&mut self, ctx: &Context) {
        if self.pass_edit.vars != store::vars_text() {
            if let Err(e) = store::save_vars(&self.pass_edit.vars) {
                self.status = tf!("No se pudieron guardar los ajustes: {e}", e = e);
            }
        }
        let s = self.settings_edit.clone();
        let java_changed = s.java != self.settings.java || s.jvm != self.settings.jvm;
        let catalog_changed = s.catalog_dir() != self.catalog.dir;
        let serve_changed = (s.serve, &s.serve_addr, &s.serve_token, s.serve_passwords, s.max_rows, s.serve_tls, &s.serve_cert, &s.serve_key)
            != (
                self.settings.serve,
                &self.settings.serve_addr,
                &self.settings.serve_token,
                self.settings.serve_passwords,
                self.settings.max_rows,
                self.settings.serve_tls,
                &self.settings.serve_cert,
                &self.settings.serve_key,
            );
        if let Err(e) = s.save() {
            self.status = tf!("No se pudieron guardar los ajustes: {e}", e = e);
        }
        apply_theme(ctx, &s);
        self.settings = s;
        apply_lang(&self.settings);
        if java_changed {
            self.disconnect();
            self.bridge = new_bridge(ctx, &self.settings);
            self.java = None;
            self.java_ok = bridge::java_available(&self.settings.java);
            self.pending.clear();
            self.test = TestState::Idle;
        }
        if catalog_changed {
            self.catalog = Catalog { dir: self.settings.catalog_dir() };
            let _ = std::fs::create_dir_all(&self.catalog.dir);
            self.form = None;
            self.selected = None;
            self.load_lock();
            self.reload();
        }
        if serve_changed || catalog_changed || java_changed {
            self.start_server();
        }
    }

    // ---------- dibujo ----------

    fn toolbar(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        let mut update_clicked = false;
        ui.horizontal(|ui| {
            if ui.button(t!("＋ Nueva")).clicked() {
                self.new_connection();
            }
            let dirty = self.form.as_ref().is_some_and(|f| f.dirty() || f.force);
            if ui.add_enabled(dirty, Button::new(t!("💾 Guardar"))).on_hover_text("Ctrl+S").clicked() {
                self.save();
            }
            let has = self.form.is_some();
            if ui.add_enabled(has, Button::new(t!("⧉ Duplicar"))).clicked() {
                self.duplicate();
            }
            if ui.add_enabled(has, Button::new(t!("🗑 Borrar"))).clicked() {
                self.confirm_delete = true;
            }
            ui.separator();
            let busy = matches!(self.test, TestState::Running { .. });
            if ui.add_enabled(has && !busy, Button::new(t!("▶ Probar conexión"))).on_hover_text("Ctrl+T").clicked() {
                self.test();
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if crate::update::SUPPORTED {
                    let new = self.updater.as_ref().map(|u| u.stage()).filter(|st| matches!(st, crate::update::Stage::Available(_) | crate::update::Stage::Done(_)));
                    let b = if new.is_some() { Button::new(RichText::new("⟳ ⬆").color(GREEN)) } else { Button::new("⟳") };
                    let hint = match new {
                        Some(crate::update::Stage::Available(v)) => tf!("Hay una versión nueva: {v}", v = v),
                        Some(crate::update::Stage::Done(_)) => t!("✔ Versión nueva instalada.").to_owned(),
                        _ => t!("Buscar actualizaciones…").to_owned(),
                    };
                    if ui.add(b).on_hover_text(hint).clicked() {
                        update_clicked = true;
                    }
                }
                if ui.button("⚙").on_hover_text(t!("Ajustes")).clicked() {
                    self.settings_edit = self.settings.clone();
                    self.pass_edit.vars = store::vars_text();
                    self.show_settings = !self.show_settings;
                }
                if ui.button(t!("⟳ Reiniciar Java")).on_hover_text(t!("Corta conexiones colgadas")).clicked() {
                    self.restart_java();
                }
                let dl = self.java_dl.as_ref().map(|s| s.lock().unwrap().clone());
                match dl {
                    Some(Dl::Running(m)) => {
                        ui.label(m);
                        ui.spinner();
                    }
                    Some(Dl::Failed(e)) => {
                        if ui.button(t!("⬇ Reintentar")).clicked() {
                            self.download_java(&ctx);
                        }
                        ui.colored_label(RED, e);
                    }
                    _ if !self.java_ok => {
                        if ui.button(t!("⬇ Descargar Java portátil")).on_hover_text(t!("JRE 21 de Eclipse Temurin (~50 MB) en la carpeta de configuración")).clicked() {
                            self.download_java(&ctx);
                        }
                        ui.colored_label(ORANGE, t!("☕ No se encuentra Java"));
                    }
                    _ => {
                        let (color, text) = match self.java.as_ref().filter(|_| self.bridge.running()) {
                            Some(v) => (GREEN, v.clone()),
                            None => (ui.visuals().weak_text_color(), t!("Java sin arrancar (arranca al probar)").to_owned()),
                        };
                        let r = ui.label(RichText::new(format!("☕ {text}")).color(color));
                        if !self.java_log.is_empty() {
                            r.on_hover_text(&self.java_log);
                        }
                    }
                }
            });
        });
        if update_clicked {
            // Si ya se sabe que hay versión nueva, abrir sin volver a comprobar.
            if self.updater.as_ref().is_some_and(|u| matches!(u.stage(), crate::update::Stage::Available(_) | crate::update::Stage::Done(_) | crate::update::Stage::Running)) {
                self.show_updater = true;
            } else {
                self.open_updater(&ctx);
            }
        }
    }

    fn sidebar(&mut self, ui: &mut Ui) {
        ui.add_space(4.0);
        ui.add(TextEdit::singleline(&mut self.filter).hint_text(t!("🔎 Filtrar")).desired_width(f32::INFINITY));
        ui.add_space(4.0);
        let q = self.filter.to_lowercase();
        let mut clicked = None;
        let mut deselect = false;
        let new_unsaved = self.form.as_ref().filter(|f| f.mtime.is_none()).map(|f| f.conn.name.clone());
        ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            if let Some(n) = &new_unsaved {
                let _ = ui.selectable_label(true, RichText::new(format!("● {n}")).italics());
            }
            let mut group: Option<&str> = None;
            for e in &self.entries {
                let c = &e.conn;
                if !q.is_empty() && ![&c.name, &c.group, &c.url].iter().any(|s| s.to_lowercase().contains(&q)) {
                    continue;
                }
                if group != Some(c.group.as_str()) {
                    group = Some(c.group.as_str());
                    if !c.group.is_empty() {
                        ui.add_space(4.0);
                        ui.label(RichText::new(&c.group).strong().small());
                    }
                }
                let sel = self.selected.as_deref() == Some(c.id.as_str()) && new_unsaved.is_none();
                let dirty = sel && self.form.as_ref().is_some_and(|f| f.dirty());
                let lock = if c.secret.is_empty() { "" } else { " 🔑" };
                let label = if dirty { format!("● {}{lock}", c.name) } else { format!("{}{lock}", c.name) };
                let r = ui.selectable_label(sel, label).on_hover_text(&c.url);
                if r.clicked() {
                    if !sel {
                        clicked = Some(c.id.clone());
                    } else if !dirty {
                        deselect = true;
                    }
                }
            }
            if self.entries.is_empty() && new_unsaved.is_none() {
                ui.weak(t!("Sin conexiones. Pulsa «＋ Nueva»."));
            }
        });
        if let Some(id) = clicked {
            self.select(id);
        } else if deselect {
            self.form = None;
            self.selected = None;
            self.test = TestState::Idle;
        }
    }

    /// Aviso de catálogo cifrado sin desbloquear.
    fn lock_banner(&mut self, ui: &mut Ui) {
        let needs = self.lock.is_some() && self.key.is_none() && self.entries.iter().any(|e| !e.conn.secret.is_empty());
        if !needs {
            return;
        }
        let mut go = false;
        Frame::group(ui.style()).fill(ui.visuals().faint_bg_color).show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(t!("🔒 Este catálogo tiene contraseñas compartidas cifradas. Frase común:")).color(ORANGE));
                let r = ui.add(TextEdit::singleline(&mut self.pass_edit.unlock).password(true).desired_width(180.0));
                go = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                ui.checkbox(&mut self.pass_edit.remember, t!("Recordar en este equipo"));
                go |= ui.button(t!("Desbloquear")).clicked();
            });
        });
        if go {
            self.unlock();
        }
        ui.add_space(4.0);
    }

    fn form_ui(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        self.lock_banner(ui);
        let mut pick_jar = false;
        let mut open_drivers = false;
        let mut want_driver = None;
        let catalog = self.catalog.dir.clone();
        let has_key = self.key.is_some();
        let driver_dl = self.driver_dl.as_ref().map(|(s, _)| s.lock().unwrap().clone());
        let Some(f) = &mut self.form else {
            ui.centered_and_justified(|ui| ui.weak(t!("Elige una conexión en el panel lateral para ver sus datos, o crea una con «＋ Nueva».")));
            return;
        };
        if f.external {
            ui.colored_label(ORANGE, t!("⚠ Otro equipo o programa cambió esta conexión mientras la editabas. Guardar la sobrescribe."));
        }
        // Ancho de los campos: lo que queda tras la columna de etiquetas (máx. 760).
        let w = (ui.available_width() - 150.0).clamp(200.0, 760.0);
        Grid::new("form").num_columns(2).spacing([10.0, 6.0]).min_col_width(130.0).show(ui, |ui| {
            ui.label(t!("Nombre"));
            ui.add(TextEdit::singleline(&mut f.conn.name).desired_width(w));
            ui.end_row();
            ui.label(t!("Grupo"));
            ui.add(TextEdit::singleline(&mut f.conn.group).hint_text(t!("p. ej. Producción")).desired_width(w));
            ui.end_row();
            ui.label(t!("Tipo"));
            ui.horizontal(|ui| {
                ComboBox::from_id_salt("plantilla").selected_text(template_for(&f.conn.url).unwrap_or(t!("Plantilla…"))).show_ui(ui, |ui| {
                    for (name, cls, url, _) in TEMPLATES {
                        if ui.selectable_label(false, *name).clicked() {
                            f.conn.url = (*url).into();
                            f.conn.driver = (*cls).into();
                        }
                    }
                });
                ui.weak(t!("rellena URL y clase"));
            });
            ui.end_row();
            ui.label(t!("URL JDBC"));
            ui.add(TextEdit::singleline(&mut f.conn.url).font(TextStyle::Monospace).hint_text("jdbc:…").desired_width(w));
            ui.end_row();
            // Con variables: cómo queda en este equipo.
            if f.conn.url.contains("${") || f.conn.user.contains("${") {
                let vars = store::load_vars();
                ui.label("");
                match (store::expand(&f.conn.url, &vars), store::expand(&f.conn.user, &vars)) {
                    (Ok(u), Ok(us)) => {
                        let who = if us.is_empty() { String::new() } else { format!("  ·  {us}") };
                        ui.weak(RichText::new(format!("→ {u}{who}")).monospace().small());
                    }
                    (Err(e), _) | (_, Err(e)) => {
                        ui.colored_label(ORANGE, RichText::new(e).small());
                    }
                }
                ui.end_row();
            }
            ui.label(t!("Usuario"));
            ui.add(TextEdit::singleline(&mut f.conn.user).desired_width(w));
            ui.end_row();
            ui.label(t!("Contraseña"));
            ui.horizontal(|ui| {
                let hint = if f.locked { t!("cifrada: desbloquea el catálogo") } else { "" };
                ui.add(TextEdit::singleline(&mut f.password).password(true).hint_text(hint).desired_width((w - 230.0).max(120.0)));
                ComboBox::from_id_salt("guardar_clave").selected_text(f.keep.label()).width(200.0).show_ui(ui, |ui| {
                    for k in [Keep::No, Keep::Local, Keep::Shared] {
                        ui.selectable_value(&mut f.keep, k, k.label());
                    }
                });
            });
            ui.end_row();
            if f.keep == Keep::Shared && !has_key {
                ui.label("");
                ui.colored_label(ORANGE, t!("Para compartirla cifrada hace falta la frase común (Ajustes)."));
                ui.end_row();
            }
            ui.label(t!("Clase del driver"));
            ui.add(TextEdit::singleline(&mut f.conn.driver).font(TextStyle::Monospace).hint_text(t!("vacío = la busca en los jar según la URL")).desired_width(w));
            ui.end_row();
            ui.label(t!("Jar del driver")).on_hover_text(t!("Uno por línea: archivos o carpetas, relativos al catálogo. Vacío = carpeta «drivers» del catálogo."));
            ui.horizontal_top(|ui| {
                ui.add(TextEdit::multiline(&mut f.jars).font(TextStyle::Monospace).hint_text(t!("vacío = <catálogo>/drivers")).desired_rows(2).desired_width((w - 90.0).max(120.0)));
                ui.vertical(|ui| {
                    if ui.button(t!("Añadir…")).clicked() {
                        pick_jar = true;
                    }
                    if ui.button("🗁").on_hover_text(t!("Abrir la carpeta de drivers descargados")).clicked() {
                        open_drivers = true;
                    }
                });
            });
            ui.end_row();
            // ¿Hay jar? Si no, ofrecer descargarlo de Maven Central.
            let jars = f.built().jar_paths(&catalog);
            let missing: Vec<_> = jars.iter().filter(|p| !p.is_file()).collect();
            if jars.is_empty() || !missing.is_empty() {
                ui.label("");
                ui.horizontal(|ui| {
                    let msg = if jars.is_empty() { t!("⚠ No hay jar del driver.").to_owned() } else { tf!("⚠ No existe: {ruta}", ruta = missing[0].display()) };
                    ui.colored_label(ORANGE, msg);
                    match &driver_dl {
                        Some(Dl::Running(m)) => {
                            ui.spinner();
                            ui.label(m);
                        }
                        Some(Dl::Failed(e)) => {
                            ui.colored_label(RED, e);
                        }
                        _ => {}
                    }
                    if let Some(t) = template_for(&f.conn.url).filter(|t| javadl::maven_coords(t).is_some()) {
                        if !matches!(driver_dl, Some(Dl::Running(_))) && ui.button(tf!("⬇ Descargar driver de {nombre}", nombre = t)).on_hover_text(t!("Desde Maven Central a la carpeta drivers del catálogo (se comparte con él)")).clicked() {
                            want_driver = Some(t);
                        }
                    }
                });
                ui.end_row();
            }
            ui.label(t!("Túnel SSH")).on_hover_text(t!("Usa el ssh del sistema (claves, agente, ~/.ssh/config). Nunca pide contraseña: hace falta una clave."));
            ui.horizontal(|ui| {
                ui.add(TextEdit::singleline(&mut f.conn.ssh).hint_text(t!("usuario@bastión[:puerto] (vacío = sin túnel)")).desired_width((w * 0.5).max(160.0)));
                if !f.conn.ssh.trim().is_empty() {
                    ui.add(TextEdit::singleline(&mut f.conn.ssh_dest).hint_text(t!("destino host:puerto (vacío = el de la URL)")).desired_width((w * 0.5 - 8.0).max(140.0)));
                }
            });
            ui.end_row();
            if !f.conn.ssh.trim().is_empty() {
                ui.label("");
                ui.horizontal(|ui| {
                    ui.add(TextEdit::singleline(&mut f.conn.ssh_key).hint_text(t!("clave privada (vacío = agente o ~/.ssh)")).desired_width((w * 0.5).max(160.0)));
                    ui.add(TextEdit::singleline(&mut f.conn.ssh_opts).hint_text(t!("opciones de ssh, p. ej. -o ProxyJump=otro")).desired_width((w * 0.5 - 8.0).max(140.0)));
                });
                ui.end_row();
            }
            ui.label(t!("Propiedades"));
            ui.add(TextEdit::multiline(&mut f.props).font(TextStyle::Monospace).hint_text(t!("clave=valor, una por línea")).desired_rows(2).desired_width(w));
            ui.end_row();
            ui.label(t!("Espera (s)"));
            ui.add(DragValue::new(&mut f.conn.timeout).range(1..=600));
            ui.end_row();
            ui.label(t!("Notas"));
            ui.add(TextEdit::multiline(&mut f.conn.notes).desired_rows(1).desired_width(w));
            ui.end_row();
        });
        if pick_jar {
            self.pick(&ctx, false);
        }
        if open_drivers {
            self.open_drivers_dir();
        }
        if let Some(t) = want_driver {
            self.download_driver(&ctx, t);
        }
    }

    /// Ventana con el resultado de «Probar conexión». Cerrarla descarta el resultado
    /// (si aún no llegó, se ignora cuando llegue).
    fn test_window(&mut self, ctx: &Context) {
        let mut open = true;
        let title = if self.test_name.trim().is_empty() { t!("Prueba de conexión").to_owned() } else { tf!("Prueba de conexión: {nombre}", nombre = self.test_name.trim()) };
        let max_h = (ctx.content_rect().height() * 0.6).max(120.0);
        let mut again = false;
        Window::new(title).id(win_id(Win::Test)).open(&mut open).collapsible(false).resizable(true).default_width(620.0).show(ctx, |ui| {
            self.test_result(ui, max_h);
            if !matches!(self.test, TestState::Running { .. }) {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    again = ui.add_enabled(self.form.is_some(), Button::new(t!("▶ Probar otra vez"))).clicked();
                    if ui.button(t!("Cerrar")).clicked() {
                        self.test = TestState::Idle;
                    }
                });
            }
        });
        if again {
            self.test();
        } else if !open {
            self.close(Win::Test);
        }
    }

    fn test_result(&mut self, ui: &mut Ui, max_h: f32) {
        match &self.test {
            TestState::Idle => {}
            TestState::Running { since, .. } => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    let s = format!("{:.1}", since.elapsed().as_secs_f32());
                    ui.label(if self.java.is_none() { tf!("Arrancando Java y conectando… {s} s", s = s) } else { tf!("Conectando… {s} s", s = s) });
                });
            }
            TestState::Ok(info) => {
                let get = |k: &str| info.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str()).unwrap_or("");
                ui.colored_label(GREEN, RichText::new(tf!("✔ Conectó en {ms} ms", ms = get("ms"))).strong());
                Grid::new("info").num_columns(2).spacing([10.0, 2.0]).show(ui, |ui| {
                    for (k, label) in [
                        ("producto", t!("Base de datos")),
                        ("version", t!("Versión")),
                        ("driver", t!("Driver")),
                        ("driver_version", t!("Versión del driver")),
                        ("jdbc", "JDBC"),
                        ("usuario", t!("Usuario")),
                        ("catalogo", t!("Catálogo")),
                        ("esquema", t!("Esquema")),
                        ("valida", "isValid"),
                    ] {
                        let v = get(k);
                        if !v.is_empty() {
                            ui.weak(label);
                            ui.label(v);
                            ui.end_row();
                        }
                    }
                });
            }
            TestState::Err(e) => error_box(ui, e, max_h),
        }
    }

    fn explorer_ui(&mut self, ui: &mut Ui) {
        let mut want: Option<Meta> = None;
        let mut insert: Option<String> = None;
        let mut import_into: Option<String> = None;
        ui.horizontal(|ui| {
            ui.strong(t!("Explorador"));
            if self.session_id().is_some() && ui.small_button("⟳").on_hover_text(t!("Recargar")).clicked() {
                want = Some(Meta::Schemas);
            }
        });
        ui.add(TextEdit::singleline(&mut self.explorer.filter).hint_text(t!("🔎 Tabla")).desired_width(f32::INFINITY));
        if self.session_id().is_none() {
            ui.weak(t!("Conecta para ver esquemas y tablas."));
        } else if self.explorer.loading {
            ui.spinner();
        }
        if let Some(e) = &self.explorer.error {
            ui.colored_label(RED, e);
        }
        let q = self.explorer.filter.to_lowercase();
        ScrollArea::vertical().id_salt("explorador").auto_shrink(false).show(ui, |ui| {
            for x in &mut self.explorer.schemas {
                let id = ui.make_persistent_id(("esquema", &x.cat, &x.sch));
                let header = CollapsingHeader::new(x.label().to_owned()).id_salt(id).open(Some(x.open || (!q.is_empty() && x.tables.is_some())));
                let r = header.show(ui, |ui| {
                    if x.loading {
                        ui.spinner();
                    }
                    if let Some(tables) = &x.tables {
                        let mut shown = 0;
                        for (name, kind) in tables {
                            if !q.is_empty() && !name.to_lowercase().contains(&q) {
                                continue;
                            }
                            shown += 1;
                            if shown > 2000 {
                                ui.weak(t!("… (filtra para ver más)"));
                                break;
                            }
                            let icon = if kind.to_ascii_uppercase().contains("VIEW") { "👁" } else { "▦" };
                            let full = if x.qualifier().is_empty() { name.clone() } else { format!("{}.{name}", x.qualifier()) };
                            let r = ui.add(Label::new(format!("{icon} {name}")).sense(Sense::click())).on_hover_text(format!("{kind}\n{}", t!("Clic: DDL · Doble clic: SELECT")));
                            if r.double_clicked() {
                                insert = Some(format!("select * from {full}"));
                            } else if r.clicked() {
                                want = Some(Meta::Ddl(full.clone(), kind.clone()));
                            }
                            r.context_menu(|ui| {
                                if ui.button(t!("Ver DDL")).clicked() {
                                    want = Some(Meta::Ddl(full.clone(), kind.clone()));
                                    ui.close();
                                }
                                if ui.button(t!("Ver columnas")).clicked() {
                                    want = Some(Meta::Columns(full.clone()));
                                    ui.close();
                                }
                                if ui.button("SELECT *").clicked() {
                                    insert = Some(format!("select * from {full}"));
                                    ui.close();
                                }
                                if ui.button(t!("Importar CSV…")).clicked() {
                                    import_into = Some(full.clone());
                                    ui.close();
                                }
                                if ui.button(t!("Copiar nombre")).clicked() {
                                    ui.ctx().copy_text(full.clone());
                                    ui.close();
                                }
                            });
                        }
                        if tables.is_empty() {
                            ui.weak(t!("(sin tablas)"));
                        }
                    }
                });
                if r.header_response.clicked() {
                    x.open = !x.open;
                    if x.open && x.tables.is_none() && !x.loading {
                        want = Some(Meta::Tables(x.cat.clone(), x.sch.clone()));
                    }
                }
            }
        });
        if let Some(sql) = insert {
            self.console.sql = sql;
            self.console.selection = None;
            self.run_sql(true);
        }
        if let Some(t) = import_into {
            self.open_import(&t);
        }
        if let Some(m) = want {
            self.meta(m);
        }
    }

    fn console_ui(&mut self, ui: &mut Ui) {
        ui.set_min_height(ui.available_height());
        Panel::left("explorador_panel").resizable(true).default_size(220.0).min_size(120.0).show(ui, |ui| self.explorer_ui(ui));
        CentralPanel::default().show(ui, |ui| self.sql_ui(ui));
    }

    fn sql_ui(&mut self, ui: &mut Ui) {
        let mut run: Option<bool> = None;
        ui.horizontal(|ui| {
            ui.strong(t!("Consola SQL"));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if self.console.session.is_some() && ui.button(t!("Desconectar")).clicked() {
                    self.disconnect();
                }
                let can = self.form.is_some() || self.console.session.is_some();
                if self.console.running.is_some() {
                    if ui.button(t!("■ Cancelar")).clicked() {
                        self.cancel_sql();
                    }
                } else {
                    if ui.add_enabled(can, Button::new(t!("⏩ Script"))).on_hover_text(t!("Todas las sentencias, en orden (Ctrl+Mayús+Enter)")).clicked() {
                        run = Some(true);
                    }
                    if ui.add_enabled(can, Button::new(t!("▶ Ejecutar"))).on_hover_text(t!("La selección o la sentencia del cursor (Ctrl+Enter)")).clicked() {
                        run = Some(false);
                    }
                }
                if ui.button(t!("⬆ Importar CSV")).clicked() {
                    self.open_import("");
                }
                if ui.selectable_label(self.console.show_history, t!("🕘 Historial")).clicked() {
                    self.console.show_history = !self.console.show_history;
                }
                ui.checkbox(&mut self.console.keep_going, t!("Seguir tras un error"));
                // Estado de la sesión en el hueco que queda (recortado si no cabe).
                match (&self.console.session, self.console.opening) {
                    (Some((_, n)), None) => {
                        ui.add(Label::new(RichText::new(format!("● {n}")).color(GREEN)).truncate());
                    }
                    (Some((_, n)), Some(_)) => {
                        ui.add(Label::new(tf!("Conectando a {nombre}…", nombre = n)).truncate());
                        ui.spinner();
                    }
                    _ => {
                        ui.add(Label::new(RichText::new(t!("sin conectar (Ejecutar conecta con la seleccionada)")).weak()).truncate());
                    }
                }
            });
        });

        // Editor con colores; la sentencia del cursor se resalta.
        let editor_id = Id::unique("sql_editor");
        if ui.memory(|m| m.has_focus(editor_id)) {
            if ui.input_mut(|i| i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::Enter)) {
                run = Some(true);
            } else if ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Enter)) {
                run = Some(false);
            }
        }
        let current = if self.console.selection.is_some() { None } else { crate::sql::at_cursor(&self.console.sql, self.console.cursor.min(self.console.sql.len())) };
        let mut layouter = |ui: &Ui, buf: &dyn TextBuffer, wrap: f32| {
            let mut job = highlight_job(buf.as_str(), ui.style(), current.clone());
            job.wrap.max_width = wrap;
            ui.fonts_mut(|f| f.layout_job(job))
        };
        let out = TextEdit::multiline(&mut self.console.sql).id(editor_id).code_editor().desired_rows(5).desired_width(f32::INFINITY).layouter(&mut layouter).show(ui);
        if let Some(cr) = out.cursor_range {
            let to_byte = |ci: usize| self.console.sql.char_indices().nth(ci).map_or(self.console.sql.len(), |(b, _)| b);
            let (a, b) = (to_byte(cr.primary.index.0), to_byte(cr.secondary.index.0));
            self.console.cursor = a;
            self.console.selection = (a != b).then(|| a.min(b)..a.max(b));
        }
        if let Some(all) = run {
            self.run_sql(all);
        }

        // Estado y pestañas de resultados.
        ui.horizontal(|ui| {
            if let Some((_, t)) = self.console.running {
                ui.spinner();
                let n = self.console.tabs.last().filter(|_| self.console.current_tab).map_or(0, |t| t.rows.len());
                let left = self.console.queue.len();
                let mut msg = tf!("Ejecutando… {n} filas · {s} s", n = n, s = format!("{:.1}", t.elapsed().as_secs_f32()));
                if left > 0 {
                    msg.push_str(&tf!(" (quedan {n})", n = left));
                }
                ui.label(msg);
            } else {
                ui.weak(&self.console.info);
            }
        });
        let mut close = None;
        if !self.console.log.is_empty() || !self.console.tabs.is_empty() {
            ScrollArea::horizontal().id_salt("pestanas").max_height(28.0).show(ui, |ui| {
                ui.horizontal(|ui| {
                    if !self.console.log.is_empty() {
                        let errors = self.console.log.iter().filter(|(ok, _)| !ok).count();
                        let label = if errors > 0 { RichText::new(tf!("Mensajes ({n} ✖)", n = errors)).color(RED) } else { RichText::new(tf!("Mensajes ({n})", n = self.console.log.len())) };
                        if ui.selectable_label(self.console.active.is_none(), label).clicked() {
                            self.console.active = None;
                        }
                    }
                    for (i, t) in self.console.tabs.iter().enumerate() {
                        if ui.selectable_label(self.console.active == Some(i), &t.title).on_hover_text(&t.info).clicked() {
                            self.console.active = Some(i);
                        }
                        if ui.small_button("✕").clicked() {
                            close = Some(i);
                        }
                        ui.add_space(4.0);
                    }
                });
            });
        }
        if let Some(i) = close {
            let c = &mut self.console;
            if !(c.running.is_some() && c.current_tab && i + 1 == c.tabs.len()) {
                c.tabs.remove(i);
                c.active = if c.tabs.is_empty() { None } else { Some(i.min(c.tabs.len() - 1)) };
            }
        }
        match self.console.active {
            None => {
                if let Some(e) = &self.console.error {
                    let h = ui.available_height() * 0.6;
                    error_box(ui, e, h);
                }
                ScrollArea::vertical().id_salt("mensajes").auto_shrink(false).show(ui, |ui| {
                    for (ok, line) in &self.console.log {
                        ui.colored_label(if *ok { ui.visuals().text_color() } else { RED }, RichText::new(line).monospace());
                    }
                });
            }
            Some(_) => {
                let running = self.console.running.is_some() && self.console.current_tab && self.console.active == Some(self.console.tabs.len() - 1);
                let mut copy = false;
                let mut want_export = false;
                let mut to_editor = None;
                if let Some(text) = self.console.tab().and_then(|t| t.text.clone()) {
                    let info = self.console.tab().map(|t| t.info.clone()).unwrap_or_default();
                    ui.horizontal(|ui| {
                        ui.weak(info);
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.small_button(t!("Al editor")).on_hover_text(t!("Copia la DDL al editor SQL")).clicked() {
                                to_editor = Some(text.clone());
                            }
                            if ui.small_button(t!("📋 Copiar")).clicked() {
                                ui.ctx().copy_text(text.clone());
                                self.status = t!("DDL copiada.").into();
                            }
                        });
                    });
                    ScrollArea::both().id_salt("ddl").auto_shrink(false).show(ui, |ui| {
                        let job = highlight_job(&text, ui.style(), None);
                        ui.add(Label::new(job).selectable(true).extend());
                    });
                    if let Some(t) = to_editor {
                        self.console.sql = t;
                        self.console.selection = None;
                    }
                    return;
                }
                if let Some(t) = self.console.tab() {
                    ui.horizontal(|ui| {
                        ui.weak(&t.info);
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.small_button(t!("⬇ Exportar…")).clicked() {
                                want_export = true;
                            }
                            if !t.rows.is_empty() && ui.small_button(t!("Copiar como TSV")).clicked() {
                                copy = true;
                            }
                        });
                    });
                    if copy {
                        ui.ctx().copy_text(tsv(&t.cols, &t.rows));
                    }
                    if let Some(msg) = results_table(ui, t, running) {
                        self.status = msg;
                    }
                }
                if copy {
                    self.status = t!("Resultado copiado.").into();
                }
                if want_export {
                    self.open_export();
                }
            }
        }
    }

    fn history_window(&mut self, ctx: &Context) {
        let conn = if self.console.session_conn.is_empty() { self.selected.clone().unwrap_or_default() } else { self.console.session_conn.clone() };
        let entries = self.history(&conn);
        let mut open = self.console.show_history;
        let mut pick: Option<(String, bool)> = None;
        Window::new(t!("Historial de SQL")).id(win_id(Win::History)).open(&mut open).default_size([520.0, 420.0]).show(ctx, |ui| {
            ui.add(TextEdit::singleline(&mut self.console.history_filter).hint_text(t!("🔎 Filtrar")).desired_width(f32::INFINITY));
            ui.weak(t!("Clic: copiar al editor · Doble clic: ejecutar"));
            let q = self.console.history_filter.to_lowercase();
            let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs());
            ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                let mut shown = 0;
                for (t, sql) in entries.iter().rev() {
                    if !q.is_empty() && !sql.to_lowercase().contains(&q) {
                        continue;
                    }
                    shown += 1;
                    ui.horizontal(|ui| {
                        ui.weak(RichText::new(ago(now.saturating_sub(*t))).small());
                        let r = ui.add(Label::new(RichText::new(short_title(sql)).monospace()).sense(Sense::click())).on_hover_text(sql);
                        if r.double_clicked() {
                            pick = Some((sql.clone(), true));
                        } else if r.clicked() {
                            pick = Some((sql.clone(), false));
                        }
                    });
                }
                if shown == 0 {
                    ui.weak(t!("Sin entradas."));
                }
            });
        });
        self.console.show_history = open;
        if let Some((sql, now)) = pick {
            self.console.sql = sql;
            self.console.selection = None;
            self.console.cursor = 0;
            if now {
                self.run_sql(true);
            }
        }
    }

    fn settings_window(&mut self, ctx: &Context) {
        let mut open = self.show_settings;
        let mut apply = false;
        let mut pick = false;
        let mut action: Option<u8> = None;
        let mut open_updater = false;
        let mut open_drivers = false;
        let drivers = self.catalog.dir.join("drivers");
        Window::new(t!("Ajustes")).id(win_id(Win::Settings)).open(&mut open).collapsible(false).resizable(false).default_width(560.0).show(ctx, |ui| {
            ScrollArea::vertical().min_scrolled_height((ctx.content_rect().height() - 140.0).min(720.0)).max_height(ctx.content_rect().height() - 120.0).show(ui, |ui| {
                let s = &mut self.settings_edit;
                ui.heading(t!("General"));
                Grid::new("ajustes").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                    ui.label(t!("Catálogo")).on_hover_text(t!("Carpeta de las conexiones. Ponla en una carpeta compartida (red, git, Syncthing…) para usarlas desde otros equipos."));
                    ui.horizontal(|ui| {
                        let default = store::Settings::default().catalog_dir().display().to_string();
                        ui.add(TextEdit::singleline(&mut s.catalog).hint_text(default).desired_width(320.0));
                        if ui.button(t!("Elegir…")).clicked() {
                            pick = true;
                        }
                    });
                    ui.end_row();
                    ui.label(t!("Drivers"));
                    ui.horizontal(|ui| {
                        let path = drivers.display().to_string();
                        ui.allocate_ui(vec2(320.0, ui.spacing().interact_size.y), |ui| {
                            ui.add(Label::new(RichText::new(&path).weak()).truncate()).on_hover_text(&path);
                        });
                        if ui.button(t!("🗁 Abrir carpeta")).on_hover_text(t!("Abrir la carpeta de drivers descargados")).clicked() {
                            open_drivers = true;
                        }
                    });
                    ui.end_row();
                    ui.label("Java");
                    ui.add(TextEdit::singleline(&mut s.java).hint_text(t!("vacío = portátil, JAVA_HOME o el del PATH")).desired_width(320.0));
                    ui.end_row();
                    ui.label(t!("Opciones JVM"));
                    ui.add(TextEdit::singleline(&mut s.jvm).hint_text("-Xmx384m").desired_width(320.0));
                    ui.end_row();
                    ui.label(t!("Filas máximas"));
                    ui.add(DragValue::new(&mut s.max_rows).range(1..=1_000_000));
                    ui.end_row();
                    ui.label(t!("Tema"));
                    ui.horizontal(|ui| {
                        ui.radio_value(&mut s.dark, None, t!("Sistema"));
                        ui.radio_value(&mut s.dark, Some(false), t!("Claro"));
                        ui.radio_value(&mut s.dark, Some(true), t!("Oscuro"));
                    });
                    ui.end_row();
                    ui.label(t!("Idioma"));
                    ui.horizontal(|ui| {
                        ui.radio_value(&mut s.lang, None, t!("Sistema"));
                        for l in Lang::ALL {
                            ui.radio_value(&mut s.lang, Some(l.code().to_owned()), l.name());
                        }
                    });
                    ui.end_row();
                });
                ui.weak(tf!("Java usado: {ruta}", ruta = bridge::java_path(&s.java).display()));
                ui.horizontal(|ui| {
                    ui.label(tf!("Versión {v}", v = crate::update::version_label()));
                    if crate::update::SUPPORTED && ui.button(t!("⟳ Buscar actualizaciones…")).clicked() {
                        open_updater = true;
                    }
                    ui.checkbox(&mut s.check_updates, t!("Avisar al arrancar"));
                });

                ui.separator();
                ui.heading(t!("Variables de este equipo"));
                ui.label(t!("NOMBRE=valor, una por línea. En el catálogo compartido usa ${VAR}, ${env:VAR} o ${VAR:-x} (x = valor por defecto) en URL, usuario, jar o propiedades: cada equipo pone sus valores."));
                ui.add(TextEdit::multiline(&mut self.pass_edit.vars).font(TextStyle::Monospace).hint_text("DB_HOST=localhost\nDB_PUERTO=5433").desired_rows(3).desired_width(f32::INFINITY));

                ui.separator();
                ui.heading(t!("Frase común"));
                ui.label(t!("Cifra las contraseñas compartidas del catálogo. Todos los equipos usan la misma frase; cada uno la recuerda por su cuenta."));
                let p = &mut self.pass_edit;
                match (&self.lock, &self.key) {
                    (Some(_), None) => {
                        ui.horizontal(|ui| {
                            ui.label(t!("Frase actual"));
                            ui.add(TextEdit::singleline(&mut p.unlock).password(true).desired_width(220.0));
                            if ui.button(t!("Desbloquear")).clicked() {
                                action = Some(1);
                            }
                        });
                    }
                    (Some(_), Some(_)) => {
                        ui.horizontal(|ui| {
                            ui.colored_label(GREEN, t!("🔓 Desbloqueado en este equipo"));
                            if ui.button(t!("Olvidar en este equipo")).clicked() {
                                action = Some(3);
                            }
                        });
                    }
                    (None, _) => {
                        ui.weak(t!("Aún no hay frase en este catálogo."));
                    }
                }
                if !(self.lock.is_some() && self.key.is_none()) {
                    Grid::new("frase").num_columns(2).spacing([10.0, 4.0]).show(ui, |ui| {
                        ui.label(if self.lock.is_some() { t!("Nueva frase") } else { t!("Frase") });
                        ui.add(TextEdit::singleline(&mut p.new1).password(true).desired_width(220.0));
                        ui.end_row();
                        ui.label(t!("Repítela"));
                        ui.add(TextEdit::singleline(&mut p.new2).password(true).desired_width(220.0));
                        ui.end_row();
                    });
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut p.remember, t!("Recordar en este equipo"));
                        let label = if self.lock.is_some() { t!("Cambiar frase") } else { t!("Crear frase") };
                        if ui.add_enabled(!p.new1.is_empty(), Button::new(label)).clicked() {
                            action = Some(2);
                        }
                    });
                }

                ui.separator();
                ui.heading(t!("Servicio de red"));
                ui.label(t!("Otros programas, en este u otros equipos, leen y prueban las conexiones por HTTP + JSON."));
                Grid::new("servicio").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                    ui.label(t!("Activo"));
                    ui.checkbox(&mut s.serve, t!("Servir el catálogo"));
                    ui.end_row();
                    ui.label(t!("Dirección"));
                    ui.add(TextEdit::singleline(&mut s.serve_addr).hint_text(store::DEFAULT_SERVE_ADDR).desired_width(200.0));
                    ui.end_row();
                    ui.label("Token");
                    ui.horizontal(|ui| {
                        ui.add(TextEdit::singleline(&mut s.serve_token).font(TextStyle::Monospace).desired_width(280.0));
                        if ui.button(t!("Nuevo")).clicked() {
                            s.serve_token = store::new_token();
                        }
                        if ui.button(t!("Copiar")).clicked() {
                            ui.ctx().copy_text(s.serve_token.clone());
                        }
                    });
                    ui.end_row();
                    ui.label(t!("Contraseñas"));
                    ui.checkbox(&mut s.serve_passwords, t!("Entregarlas por la red (?clave=1)"));
                    ui.end_row();
                    ui.label("TLS");
                    ui.checkbox(&mut s.serve_tls, t!("Cifrar con TLS (HTTPS)"));
                    ui.end_row();
                    if s.serve_tls {
                        ui.label(t!("Certificado"));
                        ui.add(TextEdit::singleline(&mut s.serve_cert).hint_text(t!("vacío = autofirmado (se crea solo)")).desired_width(320.0));
                        ui.end_row();
                        ui.label(t!("Clave privada"));
                        ui.add(TextEdit::singleline(&mut s.serve_key).hint_text(t!("PEM, solo con certificado propio")).desired_width(320.0));
                        ui.end_row();
                    }
                });
                if !s.serve_tls {
                    ui.colored_label(ORANGE, t!("⚠ Sin TLS todo viaja en claro (también el token): solo en una red de confianza, VPN o túnel SSH."));
                }
                let host = s.serve_addr.replace("0.0.0.0", "localhost");
                let curl = match (&self.server_cert, s.serve_tls) {
                    (Some((path, fp)), true) => {
                        ui.horizontal_wrapped(|ui| {
                            ui.weak(tf!("Huella SHA-256: {huella}", huella = fp));
                            if ui.small_button(t!("Copiar")).clicked() {
                                ui.ctx().copy_text(fp.clone());
                            }
                        });
                        ui.weak(tf!("Los clientes confían en el certificado ({ruta}) o fijan su huella.", ruta = path));
                        format!("curl --cacert '{path}' -H 'Authorization: Bearer {}' https://{host}/v1/conexiones", s.serve_token)
                    }
                    (_, true) => format!("curl --cacert <certificado> -H 'Authorization: Bearer {}' https://{host}/v1/conexiones", s.serve_token),
                    _ => format!("curl -H 'Authorization: Bearer {}' http://{host}/v1/conexiones", s.serve_token),
                };
                ui.add(Label::new(RichText::new(curl).monospace().small()).wrap());
                ui.add_space(6.0);
                if ui.button(t!("Aplicar")).clicked() {
                    apply = true;
                }
            });
        });
        if open_updater {
            self.open_updater(ctx);
        }
        if open_drivers {
            self.open_drivers_dir();
        }
        match action {
            Some(1) => {
                self.unlock();
            }
            Some(2) => self.set_passphrase(),
            Some(3) => self.forget_key(),
            _ => {}
        }
        if pick {
            self.pick(ctx, true);
        }
        if apply {
            self.apply_settings(ctx);
            open = false;
        }
        self.show_settings = open;
    }

    fn delete_window(&mut self, ctx: &Context) {
        let name = self.form.as_ref().map(|f| f.conn.name.clone()).unwrap_or_default();
        let mut close = false;
        let mut delete = false;
        Window::new(t!("Borrar conexión")).id(win_id(Win::Delete)).collapsible(false).resizable(false).anchor(Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
            ui.label(tf!("¿Borrar «{nombre}» del catálogo? Desaparece también en los otros equipos que lo compartan.", nombre = name));
            ui.horizontal(|ui| {
                if ui.button(t!("Borrar")).clicked() {
                    delete = true;
                }
                if ui.button(t!("Cancelar")).clicked() {
                    close = true;
                }
            });
        });
        if delete {
            self.delete();
        }
        if delete || close {
            self.confirm_delete = false;
        }
    }

    /// Abre `<catálogo>/drivers` (donde se descargan los drivers) en el gestor de archivos.
    fn open_drivers_dir(&mut self) {
        let dir = self.catalog.dir.join("drivers");
        if let Err(e) = std::fs::create_dir_all(&dir).and_then(|_| open_in_file_manager(&dir)) {
            self.status = tf!("No se pudo abrir {ruta}: {e}", ruta = dir.display(), e = e);
        }
    }

    fn open_updater(&mut self, ctx: &Context) {
        let running = self.updater.as_ref().is_some_and(|u| u.stage() == crate::update::Stage::Running);
        if !running {
            self.updater = Some(crate::update::Updater::check(ctx.clone()));
        }
        self.show_updater = true;
    }

    fn updater_window(&mut self, ctx: &Context) {
        use crate::update::Stage;
        let Some(u) = &self.updater else { return };
        let stage = u.stage();
        let mut open = true;
        let mut run = false;
        let mut restart: Option<std::path::PathBuf> = None;
        Window::new(t!("Actualizar conector")).id(win_id(Win::Updater)).open(&mut open).collapsible(false).default_width(560.0).show(ctx, |ui| {
            ui.label(tf!("Versión instalada: {v}", v = crate::update::version_label()));
            match &stage {
                Stage::Checking => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(t!("Buscando en GitHub…"));
                    });
                }
                Stage::UpToDate(v) => {
                    ui.colored_label(GREEN, tf!("✔ Ya tienes la última versión ({v}).", v = v));
                }
                Stage::Available(v) => {
                    ui.label(RichText::new(tf!("Hay una versión nueva: {v}", v = v)).strong());
                    ui.weak(if crate::update::WINDOWS {
                        t!("Se descarga el instalador y se ejecuta al cerrar conector.")
                    } else {
                        t!("Se descarga el código, se compila (la primera vez tarda unos minutos) y se instala. Puedes seguir usando conector mientras tanto.")
                    });
                    if ui.button(t!("⬇ Actualizar ahora")).clicked() {
                        run = true;
                    }
                }
                Stage::Running => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(t!("Actualizando… (puedes seguir trabajando)"));
                    });
                }
                Stage::Done(p) => {
                    ui.colored_label(GREEN, t!("✔ Versión nueva instalada."));
                    ui.horizontal(|ui| {
                        if ui.button(t!("⟳ Reiniciar ahora")).clicked() {
                            restart = Some(p.clone());
                        }
                        ui.weak(t!("o úsala la próxima vez que abras conector"));
                    });
                }
                Stage::Failed(e) => {
                    ui.colored_label(RED, tf!("✖ {e}", e = e));
                }
            }
            let log = u.log.lock().unwrap();
            if !log.is_empty() {
                ui.separator();
                ScrollArea::vertical().max_height(220.0).stick_to_bottom(true).auto_shrink([false, true]).show(ui, |ui| {
                    for l in log.iter() {
                        ui.label(RichText::new(l).monospace().small());
                    }
                });
            }
        });
        if run {
            u.run(ctx.clone());
        }
        if let Some(p) = restart {
            match crate::update::relaunch_after_exit(&p) {
                Ok(()) => ctx.send_viewport_cmd(ViewportCommand::Close),
                Err(e) => self.status = tf!("No se pudo abrir la versión nueva: {e}", e = e),
            }
        }
        if !open {
            self.show_updater = false;
        }
    }

    fn is_open(&self, w: Win) -> bool {
        match w {
            Win::Delete => self.confirm_delete,
            Win::Export => self.export_dialog.is_some(),
            // Con una importación en marcha la ventana no se cierra.
            Win::Import => self.import_dialog.is_some() && self.import_job.is_none(),
            Win::History => self.console.show_history,
            Win::Settings => self.show_settings,
            // Mientras instala, la ventana se queda.
            Win::Test => !matches!(self.test, TestState::Idle),
            Win::Updater => self.show_updater && !self.updater.as_ref().is_some_and(|u| u.stage() == crate::update::Stage::Running),
        }
    }

    fn close(&mut self, w: Win) {
        match w {
            Win::Delete => self.confirm_delete = false,
            Win::Export => self.export_dialog = None,
            Win::Import => self.import_dialog = None,
            Win::History => self.console.show_history = false,
            Win::Settings => self.show_settings = false,
            Win::Updater => self.show_updater = false,
            Win::Test => {
                if let TestState::Running { id, .. } = self.test {
                    self.pending.remove(&id);
                }
                self.test = TestState::Idle;
            }
        }
    }

    /// Escape cierra la ventana de delante (o la última abierta). Si hay un
    /// desplegable abierto, Escape es para él (lo cierra egui).
    fn escape_closes(&mut self, ctx: &Context) {
        if !ctx.input(|i| i.key_pressed(Key::Escape)) || ctx.any_popup_open() {
            return;
        }
        // Orden de prioridad si no se sabe cuál está delante.
        let open: Vec<Win> = [Win::Delete, Win::Updater, Win::Test, Win::Export, Win::Import, Win::History, Win::Settings].into_iter().filter(|w| self.is_open(*w)).collect();
        let Some(first) = open.first().copied() else { return };
        let top = ctx.memory(|m| m.areas().top_layer_id(Order::Middle)).map(|l| l.id);
        let target = if self.confirm_delete { Win::Delete } else { open.iter().copied().find(|w| Some(win_id(*w)) == top).unwrap_or(first) };
        self.close(target);
        // Que el Escape no llegue además a otra cosa este fotograma.
        ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape));
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        let mut cancel_export = false;
        ui.horizontal(|ui| {
            let r = ui.add(Label::new(RichText::new(format!("📁 {}", self.catalog.dir.display())).small()).sense(Sense::click()));
            if r.on_hover_text(t!("Catálogo (clic: copiar la ruta)")).clicked() {
                ui.ctx().copy_text(self.catalog.dir.display().to_string());
            }
            if self.key.is_some() {
                ui.small("🔓");
            }
            if let Some(u) = &self.updater {
                let label = match u.stage() {
                    crate::update::Stage::Available(v) => Some(tf!("⬆ Versión nueva ({v})", v = v)),
                    crate::update::Stage::Running => Some(t!("⬆ Actualizando…").to_owned()),
                    crate::update::Stage::Done(_) => Some(t!("⬆ Lista: reinicia").to_owned()),
                    _ => None,
                };
                if let Some(l) = label {
                    ui.separator();
                    if ui.add(Button::new(RichText::new(l).small().color(GREEN)).small()).clicked() {
                        self.show_updater = true;
                    }
                }
            }
            let tunnels = crate::tunnel::open_count();
            if tunnels > 0 {
                ui.separator();
                ui.small(tf!("🔐 {n} túneles SSH", n = tunnels));
            }
            match (&self.server, &self.server_err) {
                (Some(s), _) => {
                    ui.separator();
                    let lock = if self.server_cert.is_some() { "🔒 https" } else { "⚠ http" };
                    ui.colored_label(GREEN, RichText::new(tf!("📡 Sirviendo en {dir}", dir = format!("{lock}://{}", s.addr))).small());
                }
                (None, Some(e)) => {
                    ui.separator();
                    ui.colored_label(RED, RichText::new(format!("📡 {e}")).small());
                }
                _ => {}
            }
            if let Some(j) = &self.export_job {
                ui.separator();
                ui.spinner();
                let n = j.exporter.as_ref().map_or(0, |x| x.rows());
                ui.small(tf!("Exportando… {n} filas · {s} s", n = n, s = format!("{:.0}", j.started.elapsed().as_secs_f32())));
                if ui.small_button("■").on_hover_text(t!("Cancelar")).clicked() {
                    cancel_export = true;
                }
            }
            if let Some(j) = &self.import_job {
                ui.separator();
                ui.spinner();
                ui.small(tf!("Importando… {n} filas · {s} s", n = j.done_rows, s = format!("{:.0}", j.started.elapsed().as_secs_f32())));
            }
            ui.separator();
            ui.small(&self.status);
        });
        if cancel_export {
            self.cancel_export();
        }
    }

    /// Pruebas: CONECTOR_CAPTURA=ruta.rgba guarda una captura (ancho, alto u32 LE +
    /// RGBA) y sale. CONECTOR_PROBAR=1 prueba la conexión seleccionada,
    /// CONECTOR_SQL=texto la ejecuta en la consola, CONECTOR_TABLAS=esquema abre
    /// ese esquema en el explorador, CONECTOR_AJUSTES=1 abre los ajustes.
    fn debug_capture(&mut self, ctx: &Context) {
        let Ok(path) = std::env::var("CONECTOR_CAPTURA") else { return };
        self.debug_frames += 1;
        if self.debug_frames == 2 && self.form.is_none() && std::env::var_os("CONECTOR_SIN_ELEGIR").is_none() {
            if let Some(id) = self.entries.first().map(|e| e.conn.id.clone()) {
                self.select(id);
            }
        }
        if self.debug_frames == 3 {
            if std::env::var_os("CONECTOR_PROBAR").is_some() {
                self.test();
            }
            if let Ok(sql) = std::env::var("CONECTOR_SQL") {
                self.console.sql = sql;
                self.run_sql(true);
            }
            if std::env::var_os("CONECTOR_AJUSTES").is_some() {
                self.show_settings = true;
            }
            if std::env::var_os("CONECTOR_HISTORIAL").is_some() {
                self.console.show_history = true;
            }
            if std::env::var_os("CONECTOR_BORRAR").is_some() {
                self.confirm_delete = true;
            }
            if std::env::var_os("CONECTOR_ACTUALIZAR").is_some() {
                self.open_updater(ctx);
            }
        }
        if let Ok(sch) = std::env::var("CONECTOR_TABLAS") {
            if let Some(x) = self.explorer.schemas.iter_mut().find(|x| x.label().eq_ignore_ascii_case(&sch)).filter(|x| x.tables.is_none() && !x.loading) {
                x.open = true;
                let req = Meta::Tables(x.cat.clone(), x.sch.clone());
                self.meta(req);
                self.explorer.filter = std::env::var("CONECTOR_FILTRO_TABLA").unwrap_or_default();
            }
        }
        // CONECTOR_DDL=tabla pide su DDL (como un clic en el explorador) cuando estén las tablas.
        if let Ok(want) = std::env::var("CONECTOR_DDL") {
            let asked = self.console.tabs.iter().any(|t| t.text.is_some()) || self.pending.values().any(|p| matches!(p, Pending::Meta(Meta::Ddl(..))));
            let found = self.explorer.schemas.iter().find_map(|x| {
                let (n, k) = x.tables.as_ref()?.iter().find(|(n, _)| n.eq_ignore_ascii_case(&want))?;
                Some((if x.qualifier().is_empty() { n.clone() } else { format!("{}.{n}", x.qualifier()) }, k.clone()))
            });
            if let (false, Some((full, kind))) = (asked, found) {
                self.meta(Meta::Ddl(full, kind));
            }
        }
        // CONECTOR_EXPORTAR=ruta.ext exporta TODO el resultado visible al terminar el SQL;
        // CONECTOR_IMPORTAR=ruta.csv|tabla importa ese CSV.
        if self.debug_frames > 3 && self.console.running.is_none() && self.console.queue.is_empty() && self.console.opening.is_none() && !self.console.queued {
            if let Some(p) = std::env::var_os("CONECTOR_EXPORTAR").filter(|_| self.export_job.is_none() && self.debug_frames < 10_000 && !self.console.tabs.is_empty()) {
                self.open_export();
                if let Some(d) = &mut self.export_dialog {
                    let p = std::path::PathBuf::from(p);
                    d.fmt = crate::export::Format::ALL.into_iter().find(|f| p.extension().is_some_and(|e| e == f.ext())).unwrap_or(crate::export::Format::Csv);
                    d.path = p.display().to_string();
                    d.all = true;
                }
                self.start_export();
                self.debug_frames = 10_000;
            }
            if let Some((p, t)) = std::env::var("CONECTOR_IMPORTAR").ok().and_then(|v| v.split_once('|').map(|(a, b)| (a.to_owned(), b.to_owned()))) {
                if self.import_job.is_none() && self.debug_frames < 20_000 && self.session_id().is_some() {
                    self.open_import(&t);
                    if let Some(d) = &mut self.import_dialog {
                        d.path = p;
                        Self::import_preview(d, true);
                    }
                    self.start_import();
                    self.debug_frames = 20_000;
                }
            }
        }
        let busy = self.updater.as_ref().is_some_and(|u| u.stage() == crate::update::Stage::Checking)
            || self.export_job.is_some()
            || self.import_job.is_some()
            || matches!(self.test, TestState::Running { .. })
            || self.console.running.is_some()
            || self.console.opening.is_some()
            || self.console.queued
            || self.pending.values().any(|p| matches!(p, Pending::Meta(_)));
        if self.debug_frames < 30 || (busy && (self.debug_frames < 900 || (self.debug_frames >= 10_000 && self.debug_frames < 30_000))) {
            ctx.request_repaint();
            return;
        }
        if self.debug_frames == u32::MAX / 2 {
            return;
        }
        if (std::env::var_os("CONECTOR_EXPORTAR").is_some() && self.debug_frames < 10_000) || (std::env::var_os("CONECTOR_IMPORTAR").is_some() && self.debug_frames < 20_000 && self.debug_frames > 900) {
            ctx.request_repaint();
            return;
        }
        self.debug_frames = u32::MAX / 2;
        ctx.send_viewport_cmd(ViewportCommand::Screenshot(ScreenshotCallback::new(move |img| {
            let mut out = Vec::with_capacity(8 + img.pixels.len() * 4);
            out.extend_from_slice(&(img.size[0] as u32).to_le_bytes());
            out.extend_from_slice(&(img.size[1] as u32).to_le_bytes());
            out.extend_from_slice(img.as_raw());
            std::fs::write(&path, out).unwrap();
            std::process::exit(0);
        })));
    }
}

/// «hace 5 min» a partir de segundos.
fn ago(secs: u64) -> String {
    match secs {
        0..60 => t!("ahora").to_owned(),
        60..3600 => tf!("hace {n} min", n = secs / 60),
        3600..86400 => tf!("hace {n} h", n = secs / 3600),
        _ => tf!("hace {n} d", n = secs / 86400),
    }
}

/// Colores del editor SQL; `current` = sentencia del cursor (fondo tenue).
fn highlight_job(text: &str, style: &Style, current: Option<std::ops::Range<usize>>) -> text::LayoutJob {
    use crate::sql::Tok;
    let font = TextStyle::Monospace.resolve(style);
    let dark = style.visuals.dark_mode;
    let color = |t: Tok| match (t, dark) {
        (Tok::Keyword, true) => Color32::from_rgb(110, 170, 240),
        (Tok::Keyword, false) => Color32::from_rgb(20, 80, 200),
        (Tok::Str, true) => Color32::from_rgb(220, 160, 120),
        (Tok::Str, false) => Color32::from_rgb(170, 40, 30),
        (Tok::Number, true) => Color32::from_rgb(180, 210, 160),
        (Tok::Number, false) => Color32::from_rgb(20, 130, 80),
        (Tok::Comment, true) => Color32::from_rgb(120, 150, 110),
        (Tok::Comment, false) => Color32::from_rgb(90, 130, 80),
        (Tok::Plain, _) => style.visuals.text_color(),
    };
    let bg = style.visuals.faint_bg_color.gamma_multiply(if dark { 2.5 } else { 1.5 });
    let mut job = text::LayoutJob::default();
    let cur = current.unwrap_or(0..0);
    for (r, tok) in crate::sql::tokens(text) {
        // Partir en los bordes de la sentencia actual para pintar su fondo.
        let mut cuts = vec![r.start, r.end];
        for b in [cur.start, cur.end] {
            if b > r.start && b < r.end && text.is_char_boundary(b) {
                cuts.push(b);
            }
        }
        cuts.sort();
        for w in cuts.windows(2) {
            let inside = !cur.is_empty() && w[0] >= cur.start && w[1] <= cur.end;
            let fmt = TextFormat { font_id: font.clone(), color: color(tok), background: if inside { bg } else { Color32::TRANSPARENT }, ..Default::default() };
            job.append(&text[w[0]..w[1]], 0.0, fmt);
        }
    }
    job
}

/// Abre una carpeta con el gestor de archivos del sistema (sin esperar).
fn open_in_file_manager(dir: &std::path::Path) -> std::io::Result<()> {
    let prog = if cfg!(target_os = "windows") {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let mut child = std::process::Command::new(prog).arg(dir).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn()?;
    // Recoge el proceso al terminar para no dejar zombis.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Ventanas que Escape puede cerrar.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Win {
    Delete,
    Export,
    Import,
    History,
    Settings,
    Updater,
    Test,
}

fn win_id(w: Win) -> Id {
    Id::unique(("ventana", w as u8))
}

fn split_cols(m: &Msg) -> Vec<(String, String)> {
    m.fields
        .iter()
        .map(|f| {
            let f = f.as_deref().unwrap_or("");
            let (n, t) = f.split_once('\u{1}').unwrap_or((f, ""));
            (n.to_owned(), t.to_owned())
        })
        .collect()
}

/// Error completo: mensaje y detalle (SQLState, clase, causas) siempre visibles,
/// seleccionables, con desplazamiento propio si es largo y botón para copiarlo.
fn error_box(ui: &mut Ui, e: &ErrInfo, max_h: f32) {
    Frame::group(ui.style()).stroke(Stroke::new(1.0, RED.gamma_multiply(0.6))).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.colored_label(RED, RichText::new(t!("✖ Error")).strong());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.small_button(t!("📋 Copiar")).on_hover_text(t!("Copia el error completo")).clicked() {
                    let text = if e.detail.is_empty() { e.msg.clone() } else { format!("{}\n{}", e.msg, e.detail) };
                    ui.ctx().copy_text(text);
                }
            });
        });
        ScrollArea::vertical().id_salt(("error", &e.msg)).max_height(max_h.max(60.0)).auto_shrink([false, true]).show(ui, |ui| {
            ui.add(Label::new(RichText::new(&e.msg).strong()).wrap().selectable(true));
            if !e.detail.is_empty() {
                ui.add_space(4.0);
                ui.add(Label::new(RichText::new(&e.detail).monospace()).wrap().selectable(true));
            }
        });
    });
}

fn tsv(cols: &[(String, String)], rows: &[Box<[Option<Box<str>>]>]) -> String {
    let clean = |s: &str| s.replace(['\t', '\n', '\r'], " ");
    let mut out = cols.iter().map(|(n, _)| clean(n)).collect::<Vec<_>>().join("\t");
    out.push('\n');
    for r in rows {
        let line: Vec<String> = r.iter().map(|c| c.as_deref().map(clean).unwrap_or_default()).collect();
        out.push_str(&line.join("\t"));
        out.push('\n');
    }
    out
}

/// Tabla de resultados: cabecera + filas virtualizadas. Clic en una celda la copia.
fn results_table(ui: &mut Ui, c: &mut ResultTab, running: bool) -> Option<String> {
    let font = TextStyle::Monospace.resolve(ui.style());
    let char_w = ui.fonts_mut(|f| f.glyph_width(&font, '0'));
    if c.widths.len() != c.cols.len() || (running && c.rows.len() <= 200) {
        // Ancho por columna según la cabecera y las primeras 200 filas.
        c.widths = c
            .cols
            .iter()
            .enumerate()
            .map(|(i, (n, _))| {
                let longest = c.rows.iter().take(200).map(|r| r.get(i).and_then(|v| v.as_deref()).map_or(4, |v| v.chars().count().min(60))).max().unwrap_or(0);
                ((longest.max(n.chars().count()) as f32) * char_w + 16.0).clamp(48.0, 420.0)
            })
            .collect();
    }
    let row_h = ui.text_style_height(&TextStyle::Monospace) + 4.0;
    let total_w: f32 = c.widths.iter().sum::<f32>() + 48.0;
    let mut copied = None;
    let stripe = ui.visuals().faint_bg_color;
    let weak = ui.visuals().weak_text_color();
    ScrollArea::horizontal().id_salt("res_h").auto_shrink(false).show(ui, |ui| {
        ui.set_min_width(total_w);
        // Cabecera.
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            ui.add_sized([48.0, row_h], Label::new(RichText::new("#").weak()));
            for ((n, t), w) in c.cols.iter().zip(&c.widths) {
                let r = boxed(ui, *w, row_h, Label::new(RichText::new(n).strong().monospace()).truncate());
                if !t.is_empty() {
                    r.on_hover_text(t);
                }
            }
        });
        ui.separator();
        ScrollArea::vertical().id_salt("res_v").auto_shrink(false).show_rows(ui, row_h, c.rows.len(), |ui, range| {
            ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
            for i in range {
                let row = &c.rows[i];
                // Fondo de filas alternas: se reserva antes para quedar debajo del texto.
                let bg = ui.painter().add(Shape::Noop);
                let r = ui.horizontal(|ui| {
                    ui.add_sized([48.0, row_h], Label::new(RichText::new((i + 1).to_string()).color(weak).small()));
                    for (j, w) in c.widths.iter().enumerate() {
                        let cell = row.get(j).and_then(|v| v.as_deref());
                        let text = match cell {
                            Some(v) => RichText::new(v.replace(['\n', '\r', '\t'], " ")).monospace(),
                            None => RichText::new("NULL").italics().color(weak),
                        };
                        let r = boxed(ui, *w, row_h, Label::new(text).truncate().sense(Sense::click()));
                        if r.clicked() {
                            ui.ctx().copy_text(cell.unwrap_or("").to_owned());
                            copied = Some(tf!("Copiado: fila {n}, {columna}", n = i + 1, columna = c.cols[j].0));
                        }
                    }
                });
                if i % 2 == 1 {
                    ui.painter().set(bg, Shape::rect_filled(r.response.rect, 0.0, stripe));
                }
            }
        });
    });
    copied
}

/// Un widget en una caja de ancho fijo, alineado a la izquierda.
fn boxed(ui: &mut Ui, w: f32, h: f32, widget: impl Widget) -> Response {
    ui.allocate_ui_with_layout(vec2(w, h), Layout::left_to_right(Align::Center), |ui| {
        ui.set_min_size(vec2(w, h));
        ui.add_space(4.0);
        ui.add(widget)
    })
    .inner
}

impl eframe::App for App {
    fn on_exit(&mut self) {
        crate::tunnel::close_all();
    }

    /// Pruebas: CONECTOR_TECLAS='40:esc,60:esc' pulsa Escape en esos fotogramas.
    fn raw_input_hook(&mut self, _ctx: &Context, raw: &mut RawInput) {
        let Ok(keys) = std::env::var("CONECTOR_TECLAS") else { return };
        for k in keys.split(',') {
            if let Some((f, "esc")) = k.split_once(':') {
                if f.trim().parse() == Ok(self.debug_frames) {
                    for pressed in [true, false] {
                        raw.events.push(eframe::egui::Event::Key { key: Key::Escape, physical_key: None, pressed, repeat: false, modifiers: Modifiers::NONE });
                    }
                }
            }
        }
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_bridge();
        self.check_timeouts(&ctx);
        self.debug_capture(&ctx);
        self.escape_closes(&ctx);
        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::S)) {
            self.save();
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::T)) && self.form.is_some() {
            self.test();
        }

        Panel::top("barra").show(ui, |ui| {
            ui.add_space(3.0);
            self.toolbar(ui);
            ui.add_space(1.0);
        });
        Panel::bottom("estado").show(ui, |ui| self.status_bar(ui));
        Panel::left("lista").resizable(true).default_size(230.0).min_size(150.0).show(ui, |ui| self.sidebar(ui));
        Panel::bottom("consola").resizable(true).default_size(400.0).min_size(160.0).show(ui, |ui| self.console_ui(ui));
        CentralPanel::default().show(ui, |ui| {
            ScrollArea::vertical().id_salt("form_scroll").auto_shrink(false).show(ui, |ui| self.form_ui(ui));
        });
        if !matches!(self.test, TestState::Idle) {
            self.test_window(&ctx);
        }
        if self.show_settings {
            self.settings_window(&ctx);
        }
        if self.confirm_delete {
            self.delete_window(&ctx);
        }
        if self.console.show_history {
            self.history_window(&ctx);
        }
        if self.export_dialog.is_some() {
            self.export_window(&ctx);
        }
        if self.import_dialog.is_some() {
            self.import_window(&ctx);
        }
        if self.show_updater {
            self.updater_window(&ctx);
        }
    }
}

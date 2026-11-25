//! Interfaz: lista de conexiones (catálogo), formulario, prueba de conexión,
//! consola SQL con explorador de esquemas y resultados virtualizados (solo se
//! dibujan las filas visibles; las celdas se guardan como `Box<str>`).
//! También: frase común (contraseñas compartidas cifradas), descargas (Java
//! portátil, drivers de Maven) y el servicio de red para otros programas.

use crate::bridge::{self, Bridge, Event};
use crate::crypto::{self, Key as CKey, Lock};
use crate::icons::{Icon, IconButton, Tone};
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
    ("PostgreSQL", "org.postgresql.Driver", "jdbc:postgresql://localhost:5432/", "jdbc:postgresql:"),
    ("MySQL", "com.mysql.cj.jdbc.Driver", "jdbc:mysql://localhost:3306/", "jdbc:mysql:"),
    ("MariaDB", "org.mariadb.jdbc.Driver", "jdbc:mariadb://localhost:3306/", "jdbc:mariadb:"),
    ("Oracle", "oracle.jdbc.OracleDriver", "jdbc:oracle:thin:@//localhost:1521/", "jdbc:oracle:"),
    ("SQL Server", "com.microsoft.sqlserver.jdbc.SQLServerDriver", "jdbc:sqlserver://localhost:1433;encrypt=true;trustServerCertificate=true", "jdbc:sqlserver:"),
    ("DB2", "com.ibm.db2.jcc.DB2Driver", "jdbc:db2://localhost:50000/", "jdbc:db2:"),
    ("Firebird", "org.firebirdsql.jdbc.FBDriver", "jdbc:firebirdsql://localhost:3050/", "jdbc:firebirdsql:"),
    ("H2", "org.h2.Driver", "jdbc:h2:mem:prueba", "jdbc:h2:"),
    ("SQLite", "org.sqlite.JDBC", "jdbc:sqlite:", "jdbc:sqlite:"),
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
    /// Filas/columnas seleccionadas para copiar.
    sel: TableSel,
    /// Modo edición («✎ Editar»), si la consulta es de una tabla con clave.
    edit: Option<EditState>,
    /// Petición «editable» en curso para esta pestaña.
    edit_req: Option<u64>,
    /// Por qué no se puede editar (si se preguntó y no).
    read_only: Option<String>,
}

/// Edición de los valores de una pestaña de resultados.
struct EditState {
    /// Conexión de la sesión en la que se abrió (no aplicar en otra).
    conn: String,
    cat: String,
    sch: String,
    table: String,
    /// Tabla con comillas, para la vista previa.
    sql_table: String,
    /// Columna de la tabla de cada columna del resultado (None = no se puede editar).
    base: Vec<Option<String>>,
    sql_cols: Vec<String>,
    /// Columnas del resultado que forman la clave.
    key: Vec<usize>,
    /// (fila, columna) → valor nuevo (None = NULL).
    changes: std::collections::BTreeMap<(usize, usize), Option<String>>,
    /// Celda en edición: (fila, columna, texto, ¿ya tiene el foco?).
    editing: Option<(usize, usize, String, bool)>,
    /// Celda en la ventana de edición de varias líneas: (fila, columna, texto).
    big: Option<(usize, usize, String)>,
    /// Petición «editar» en curso.
    applying: Option<u64>,
    error: Option<ErrInfo>,
}

impl EditState {
    /// ¿Se puede editar la celda? (no la clave, ni calculadas/binarias, ni valores recortados por Java).
    fn can_edit(&self, row: &[Option<Box<str>>], j: usize) -> bool {
        let cut = row.get(j).and_then(|v| v.as_deref()).is_some_and(|v| v.ends_with('…') && v.chars().count() > 4096);
        self.base.get(j).is_some_and(|b| b.is_some()) && !self.key.contains(&j) && !cut
    }

    /// Guarda en `changes` lo que se escribía en la celda en edición (si cambió).
    fn commit_editing(&mut self, rows: &[Box<[Option<Box<str>>]>]) {
        let Some((r, c, text, _)) = self.editing.take() else { return };
        if rows.get(r).and_then(|row| row.get(c)).and_then(|v| v.as_deref()) == Some(text.as_str()) {
            self.changes.remove(&(r, c));
        } else {
            self.changes.insert((r, c), Some(text));
        }
    }

    fn rows_changed(&self) -> usize {
        let mut rows: Vec<usize> = self.changes.keys().map(|(r, _)| *r).collect();
        rows.dedup();
        rows.len()
    }
}

/// ¿Tiene sentido editar el valor en una ventana de varias líneas? (textos o valores con saltos de línea)
fn multiline_worthy(type_name: &str, value: Option<&str>) -> bool {
    let t = type_name.to_uppercase();
    value.is_some_and(|v| v.contains('\n') || v.chars().count() > 60) || ["CHAR", "TEXT", "CLOB", "STRING", "JSON", "XML"].iter().any(|k| t.contains(k))
}

/// Literal SQL para la vista previa (al guardar se usan parámetros).
fn sql_literal(v: Option<&str>, type_name: &str) -> String {
    let Some(v) = v else { return "NULL".into() };
    let t = type_name.to_uppercase();
    let numeric = ["INT", "NUMERIC", "DECIMAL", "NUMBER", "FLOAT", "DOUBLE", "REAL", "SERIAL", "MONEY"].iter().any(|n| t.contains(n));
    let looks_num = !v.is_empty() && v.trim().parse::<f64>().is_ok();
    let boolean = (t.contains("BOOL") || t == "BIT") && ["true", "false"].contains(&v.to_lowercase().as_str());
    if (numeric && looks_num) || boolean { v.trim().to_owned() } else { format!("'{}'", v.replace('\'', "''")) }
}

/// Los UPDATE que se ejecutarán (uno por fila cambiada), con los valores como literales.
fn edit_preview(t: &ResultTab, e: &EditState) -> String {
    let mut out = String::new();
    let mut by_row: std::collections::BTreeMap<usize, Vec<(usize, Option<&str>)>> = Default::default();
    for ((r, c), v) in &e.changes {
        by_row.entry(*r).or_default().push((*c, v.as_deref()));
    }
    for (r, cols) in by_row {
        let set: Vec<String> = cols.iter().map(|(c, v)| format!("{} = {}", e.sql_cols[*c], sql_literal(*v, &t.cols[*c].1))).collect();
        let row = &t.rows[r];
        let cond: Vec<String> = e
            .key
            .iter()
            .map(|&k| match row.get(k).and_then(|v| v.as_deref()) {
                Some(v) => format!("{} = {}", e.sql_cols[k], sql_literal(Some(v), &t.cols[k].1)),
                None => format!("{} IS NULL", e.sql_cols[k]),
            })
            .collect();
        out.push_str(&format!("UPDATE {} SET {} WHERE {};\n", e.sql_table, set.join(", "), cond.join(" AND ")));
    }
    out
}

/// Selección en la tabla de resultados: todo (esquina «#»), filas (números) o columnas (cabeceras).
#[derive(Default)]
struct TableSel {
    all: bool,
    rows: std::collections::BTreeSet<usize>,
    cols: std::collections::BTreeSet<usize>,
    /// Última fila/columna pulsada (para Mayús+clic).
    anchor: Option<usize>,
}

impl TableSel {
    fn is_empty(&self) -> bool {
        !self.all && self.rows.is_empty() && self.cols.is_empty()
    }

    /// Clic en una fila (`row`) o columna: solo esa; Ctrl/⌘ añade o quita; Mayús, rango desde la anterior.
    fn click(&mut self, row: bool, i: usize, m: Modifiers) {
        let same_kind = if row { self.cols.is_empty() } else { self.rows.is_empty() } && !self.all;
        let anchor = self.anchor.filter(|_| same_kind);
        self.all = false;
        if row { self.cols.clear() } else { self.rows.clear() }
        let set = if row { &mut self.rows } else { &mut self.cols };
        match anchor {
            Some(a) if m.shift => {
                if !m.command {
                    set.clear();
                }
                set.extend(a.min(i)..=a.max(i));
                return; // el ancla se queda para ampliar o reducir el rango
            }
            _ if m.command => {
                if !set.remove(&i) {
                    set.insert(i);
                }
            }
            _ => {
                let only = set.len() == 1 && set.contains(&i);
                set.clear();
                if !only {
                    set.insert(i);
                }
            }
        }
        self.anchor = Some(i);
    }

    fn row(&self, i: usize) -> bool {
        self.all || self.rows.contains(&i)
    }

    fn col(&self, j: usize) -> bool {
        self.all || self.cols.contains(&j)
    }
}

/// Valor para pegar en una hoja de cálculo (TSV): entre comillas si lleva tabulador, salto o comillas.
fn tsv_field(v: &str) -> std::borrow::Cow<'_, str> {
    if v.contains(['\t', '\n', '\r', '"']) { format!("\"{}\"", v.replace('"', "\"\"")).into() } else { v.into() }
}

/// Lo seleccionado como TSV (NULL = vacío) y un mensaje para la barra de estado.
fn selection_tsv(c: &ResultTab, headers: bool) -> Option<(String, String)> {
    let sel = &c.sel;
    if sel.is_empty() {
        return None;
    }
    let rows: Vec<usize> = if sel.rows.is_empty() { (0..c.rows.len()).collect() } else { sel.rows.iter().copied().filter(|&i| i < c.rows.len()).collect() };
    let cols: Vec<usize> = if sel.cols.is_empty() { (0..c.cols.len()).collect() } else { sel.cols.iter().copied().filter(|&j| j < c.cols.len()).collect() };
    let mut out = String::new();
    if headers {
        out.push_str(&cols.iter().map(|&j| tsv_field(&c.cols[j].0)).collect::<Vec<_>>().join("\t"));
        out.push('\n');
    }
    for &i in &rows {
        let r = &c.rows[i];
        out.push_str(&cols.iter().map(|&j| r.get(j).and_then(|v| v.as_deref()).map_or("".into(), tsv_field)).collect::<Vec<_>>().join("\t"));
        out.push('\n');
    }
    Some((out, tf!("Copiadas {filas} filas × {columnas} columnas", filas = rows.len(), columnas = cols.len())))
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
    /// Alto del editor SQL (0 = el de siempre).
    editor_h: f32,
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
    /// Catálogo y esquema de la sesión (se abren solos al conectar).
    current: (String, String),
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
        match self.qualifier() {
            "" => t!("(tablas)"),
            q => q,
        }
    }
    /// Prefijo para el SQL: `esquema.` (o `catálogo.` en bases sin esquemas).
    fn qualifier(&self) -> &str {
        if self.sch.is_empty() { &self.cat } else { &self.sch }
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
    /// ¿Se puede editar el resultado? (`editable`; true = automática al terminar la consulta)
    Editable(bool),
    /// Guardar los cambios de una pestaña (`editar`).
    Edit,
    Other,
}

enum Picked {
    ExportPath(std::path::PathBuf),
    ImportPath(std::path::PathBuf),
    Catalog(std::path::PathBuf),
    Jar(Vec<std::path::PathBuf>),
    /// Archivo de base (SQLite/H2) para el campo «Archivo».
    DbFile(std::path::PathBuf),
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
    /// Grupos plegados en el panel lateral.
    folded_groups: std::collections::HashSet<String>,
    /// Posición/tamaño actuales de la ventana (se guardan al salir).
    window: Option<store::WindowState>,
    /// Monitor donde estaba la ventana la última vez (lo pone `main`).
    pub saved_monitor: Option<(f32, f32)>,
    window_checked: bool,
    /// Ventana «Acerca de» abierta (y su ícono, creado al abrirla).
    show_about: bool,
    about_icon: Option<TextureHandle>,
    /// Menú de la barra de macOS.
    #[cfg(target_os = "macos")]
    menu: crate::macmenu::MacMenu,
    /// Paneles visibles: lista de conexiones y datos de la conexión.
    pub show_sidebar: bool,
    pub show_form: bool,
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
    /// Ventana con el SQL de los cambios de la pestaña activa.
    show_preview: bool,
    settings_edit: Settings,
    pass_edit: PassEdit,
    /// `variables.txt` tal como está guardado (para saber si hay cambios en Ajustes).
    vars_applied: String,
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
    /// Ventana «Atajos de teclado» y avisos flotantes (comunes).
    show_keys: bool,
    avisos: crate::estilo::Avisos,
    /// Último `status` ya mostrado como aviso.
    status_seen: String,
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
        crate::estilo::aplicar(&cc.egui_ctx);
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
            folded_groups: Default::default(),
            window: None,
            saved_monitor: None,
            window_checked: false,
            show_about: false,
            about_icon: None,
            #[cfg(target_os = "macos")]
            menu: crate::macmenu::MacMenu::new(&cc.egui_ctx),
            show_sidebar: true,
            show_form: true,
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
            show_preview: false,
            pass_edit: PassEdit { remember: true, ..Default::default() },
            vars_applied: String::new(),
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
            show_keys: false,
            avisos: Default::default(),
            status_seen: String::new(),
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
        self.show_form = true;
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
        self.show_form = true;
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
        // El archivo se llama como la conexión: nueva → nombre libre; renombrada → se renombra el archivo.
        let mut renamed_from = None;
        if f.mtime.is_none() {
            c.id = self.catalog.free_id(&c.name);
        } else if c.id != store::file_id(&c.name) {
            let new_id = self.catalog.free_id_for(&c.name, Some(&c.id));
            if new_id != c.id {
                if let Err(e) = self.catalog.rename(&c.id, &new_id) {
                    self.status = tf!("No se pudo renombrar el archivo: {e}", e = e);
                    return;
                }
                renamed_from = Some(std::mem::replace(&mut c.id, new_id));
            }
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
        let result = self.catalog.save(&c, expected);
        if let Some(old) = &renamed_from {
            match &result {
                Ok(_) => {
                    // Lo que iba con el nombre viejo: contraseña local, historial y consola.
                    let _ = self.secrets.set(Secrets::key(&self.catalog.dir, old), None);
                    if let (Some(a), Some(b)) = (self.history_path(old), self.history_path(&c.id)) {
                        let _ = std::fs::rename(a, b);
                    }
                    if self.console.session_conn == *old {
                        self.console.session_conn = c.id.clone();
                    }
                }
                // Si no se pudo guardar, el archivo vuelve a su nombre.
                Err(_) => {
                    let _ = self.catalog.rename(&c.id, old);
                }
            }
        }
        let Some(f) = &mut self.form else { return };
        match result {
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

    /// Renombra cada `.conexion` como su conexión (lleva consigo contraseña local e historial).
    fn rename_all_files(&mut self) {
        let dirty = self.form.as_ref().is_some_and(|f| f.dirty());
        let (mut done, mut failed) = (0, 0);
        let entries: Vec<(String, String)> = self.entries.iter().map(|e| (e.conn.id.clone(), e.conn.name.clone())).collect();
        for (id, name) in entries {
            if id == store::file_id(&name) || (dirty && self.selected.as_deref() == Some(id.as_str())) {
                continue;
            }
            let new_id = self.catalog.free_id_for(&name, Some(&id));
            if new_id == id {
                continue;
            }
            if self.catalog.rename(&id, &new_id).is_err() {
                failed += 1;
                continue;
            }
            done += 1;
            let old_key = Secrets::key(&self.catalog.dir, &id);
            if let Some(p) = self.secrets.get(&old_key).map(str::to_owned) {
                let _ = self.secrets.set(Secrets::key(&self.catalog.dir, &new_id), Some(p));
                let _ = self.secrets.set(old_key, None);
            }
            if let (Some(a), Some(b)) = (self.history_path(&id), self.history_path(&new_id)) {
                let _ = std::fs::rename(a, b);
            }
            if self.console.session_conn == id {
                self.console.session_conn = new_id.clone();
            }
            if self.selected.as_deref() == Some(id.as_str()) {
                self.selected = Some(new_id.clone());
            }
        }
        self.reload();
        if let Some(id) = self.selected.clone().filter(|_| !dirty) {
            self.select(id);
        }
        self.status = if failed > 0 { tf!("Renombrados {n}; fallaron {m}.", n = done, m = failed) } else { tf!("Renombrados {n} archivos.", n = done) };
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

    /// «✎ Editar»: pregunta a Java si el resultado de la pestaña activa se puede editar.
    fn start_edit(&mut self) {
        let Some(s) = self.session_id() else {
            self.status = t!("Conecta para editar.").into();
            return;
        };
        let Some(sql) = self.console.tab().map(|t| t.sql.clone()) else { return };
        let id = self.send("editable", &[("sesion".into(), s), ("sql".into(), sql)], Pending::Editable(false));
        if let Some(t) = self.console.tab() {
            t.edit_req = Some(id);
        }
    }

    /// Al terminar un SELECT, pregunta en segundo plano si se puede editar (como DBeaver: sin modo aparte).
    fn probe_editable(&mut self) {
        let Some(s) = self.session_id() else { return };
        let Some(t) = self.console.tabs.last() else { return };
        let first = t.sql.trim_start().split(|c: char| !c.is_ascii_alphabetic()).next().unwrap_or("");
        if !first.eq_ignore_ascii_case("select") || t.cols.is_empty() || t.edit.is_some() {
            return;
        }
        let sql = t.sql.clone();
        let id = self.send("editable", &[("sesion".into(), s), ("sql".into(), sql)], Pending::Editable(true));
        if let Some(t) = self.console.tabs.last_mut() {
            t.edit_req = Some(id);
        }
    }

    /// Guarda los cambios de la pestaña activa (un UPDATE por fila, en una transacción).
    fn apply_edit(&mut self) {
        let session = self.session_id();
        let conn = self.console.session_conn.clone();
        let Some(t) = self.console.tab() else { return };
        let Some(e) = &t.edit else { return };
        if e.changes.is_empty() || e.applying.is_some() {
            return;
        }
        let Some(s) = session.filter(|_| e.conn == conn) else {
            self.status = t!("Vuelve a conectar a la conexión de este resultado para guardar.").into();
            return;
        };
        let key_names: Vec<&str> = e.key.iter().map(|&k| e.base[k].as_deref().unwrap_or("")).collect();
        let mut fields: Vec<(String, String)> = vec![
            ("sesion".into(), s),
            ("catalogo".into(), e.cat.clone()),
            ("esquema".into(), e.sch.clone()),
            ("tabla".into(), e.table.clone()),
            ("clave".into(), key_names.join("\u{1}")),
        ];
        let mut by_row: std::collections::BTreeMap<usize, Vec<(usize, Option<String>)>> = Default::default();
        for ((r, c), v) in &e.changes {
            by_row.entry(*r).or_default().push((*c, v.clone()));
        }
        let val = |v: Option<&str>| match v {
            Some(v) => ("v".to_owned(), v.to_owned()),
            None => ("z".to_owned(), String::new()),
        };
        for (r, cols) in by_row {
            let names: Vec<&str> = cols.iter().map(|(c, _)| e.base[*c].as_deref().unwrap_or("")).collect();
            fields.push(("set".into(), names.join("\u{1}")));
            for (_, v) in &cols {
                fields.push(val(v.as_deref()));
            }
            for &k in &e.key {
                fields.push(val(t.rows[r].get(k).and_then(|v| v.as_deref())));
            }
        }
        let id = self.send("editar", &fields, Pending::Edit);
        if let Some(e) = self.console.tab().and_then(|t| t.edit.as_mut()) {
            e.applying = Some(id);
            e.error = None;
        }
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
                self.pass_edit = PassEdit { remember: self.pass_edit.remember, vars: std::mem::take(&mut self.pass_edit.vars), ..Default::default() };
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
        self.pass_edit = PassEdit { remember: self.pass_edit.remember, vars: std::mem::take(&mut self.pass_edit.vars), ..Default::default() };
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
            Some(Pending::Editable(auto)) => {
                if let Some(t) = self.console.tabs.iter_mut().find(|t| t.edit_req == Some(id)) {
                    t.edit_req = None;
                    t.read_only = Some(e.msg.clone());
                }
                if !auto {
                    self.status = e.msg;
                }
            }
            Some(Pending::Edit) => {
                if let Some(ed) = self.console.tabs.iter_mut().filter_map(|t| t.edit.as_mut()).find(|ed| ed.applying == Some(id)) {
                    ed.applying = None;
                    ed.error = Some(e.clone());
                }
                self.status = e.msg;
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
            Some(&Pending::Editable(auto)) => {
                self.pending.remove(&m.id);
                let conn = self.console.session_conn.clone();
                if let Some(t) = self.console.tabs.iter_mut().find(|t| t.edit_req == Some(m.id)) {
                    t.edit_req = None;
                    let list = |k: &str| m.get(k).unwrap_or("").split('\u{1}').map(str::to_owned).collect::<Vec<_>>();
                    let base: Vec<Option<String>> = list("columnas").into_iter().map(|c| Some(c).filter(|c| !c.is_empty())).collect();
                    let key: Vec<usize> = m.get("clave").unwrap_or("").split(',').filter_map(|k| k.parse().ok()).collect();
                    if base.len() != t.cols.len() || key.is_empty() {
                        t.read_only = Some(t!("No se puede editar este resultado.").into());
                        if !auto {
                            self.status = t!("No se puede editar este resultado.").into();
                        }
                        return;
                    }
                    let table = m.get("sql_tabla").unwrap_or("").to_owned();
                    if !auto {
                        self.status = tf!("Editando {tabla}: clic en una celda para cambiarla.", tabla = table);
                    }
                    t.edit = Some(EditState {
                        conn,
                        cat: m.get("catalogo").unwrap_or("").to_owned(),
                        sch: m.get("esquema").unwrap_or("").to_owned(),
                        table: m.get("tabla").unwrap_or("").to_owned(),
                        sql_table: table,
                        base,
                        sql_cols: list("sql_columnas"),
                        key,
                        changes: Default::default(),
                        editing: None,
                        big: None,
                        applying: None,
                        error: None,
                    });
                }
            }
            Some(Pending::Edit) => {
                self.pending.remove(&m.id);
                if let Some(t) = self.console.tabs.iter_mut().find(|t| t.edit.as_ref().is_some_and(|e| e.applying == Some(m.id))) {
                    let e = t.edit.as_mut().unwrap();
                    let n = e.rows_changed();
                    for ((r, c), v) in std::mem::take(&mut e.changes) {
                        if let Some(cell) = t.rows.get_mut(r).and_then(|row| row.get_mut(c)) {
                            *cell = v.map(Into::into);
                        }
                    }
                    e.applying = None;
                    e.error = None;
                    self.status = tf!("Guardadas {n} filas en {tabla}.", n = n, tabla = e.sql_table);
                    t.widths.clear(); // los valores nuevos pueden ser más largos
                    self.show_preview = false;
                }
            }
            Some(Pending::Test) => {
                self.pending.remove(&m.id);
                if matches!(self.test, TestState::Running { id, .. } if id == m.id) {
                    self.test = TestState::Ok(m.pairs().map(|(k, v)| (k.to_owned(), v.to_owned())).collect());
                    // Conectó: abrir la sesión de la consola para ver las tablas en el explorador.
                    let this = self.form.as_ref().map(|f| f.conn.id.clone());
                    let busy = self.console.running.is_some() || self.console.opening.is_some() || self.import_job.is_some() || self.export_job.is_some();
                    let same = self.console.session.is_some() && this.as_deref() == Some(self.console.session_conn.as_str());
                    if !busy && !same && this.is_some() {
                        self.connect();
                    }
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
                self.explorer.current = (m.get("catalogo").unwrap_or("").to_owned(), m.get("esquema").unwrap_or("").to_owned());
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
                        let mut probe = false;
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
                            probe = true;
                        } else {
                            let info = if upd.is_empty() { tf!("Hecho en {ms} ms", ms = ms) } else { tf!("Filas afectadas: {n} · {ms} ms", n = upd, ms = ms) };
                            c.log.push((true, format!("✔ {title} — {info}")));
                            if c.tabs.is_empty() {
                                c.active = None;
                            }
                        }
                        c.running = None;
                        let more = !c.queue.is_empty();
                        if probe {
                            self.probe_editable();
                        }
                        if more {
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
                // Se abre solo el esquema de la sesión (o el único que haya) con sus tablas.
                // Sin esquemas ni catálogos (SQLite): uno sin nombre con todas las tablas.
                if self.explorer.schemas.is_empty() {
                    self.explorer.schemas.push(Schema { cat: String::new(), sch: String::new(), tables: None, loading: false, open: false });
                }
                let (cur_cat, cur_sch) = self.explorer.current.clone();
                let eq = |a: &str, b: &str| !a.is_empty() && a.eq_ignore_ascii_case(b);
                let only = self.explorer.schemas.len() == 1;
                let pick = self.explorer.schemas.iter().position(|x| eq(&cur_sch, &x.sch) && (cur_cat.is_empty() || x.cat.is_empty() || x.cat.eq_ignore_ascii_case(&cur_cat)))
                    .or_else(|| self.explorer.schemas.iter().position(|x| x.sch.is_empty() && eq(&cur_cat, &x.cat)))
                    .or(if only { Some(0) } else { None });
                if let Some(i) = pick {
                    let x = &mut self.explorer.schemas[i];
                    x.open = true;
                    let req = Meta::Tables(x.cat.clone(), x.sch.clone());
                    self.meta(req);
                }
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
                self.console.push_tab(ResultTab { title: format!("▦ {label}"), sql: String::new(), cols, rows, widths: Vec::new(), info, ..Default::default() });
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
                Picked::DbFile(file) => {
                    if let Some(f) = &mut self.form {
                        if let Some(mut p) = crate::urlparts::parse(&f.conn.url) {
                            let mut path = file.display().to_string();
                            // H2 no lleva la extensión del archivo en la URL.
                            if p.prefix == "jdbc:h2:" {
                                path = path.trim_end_matches(".mv.db").trim_end_matches(".h2.db").to_owned();
                            }
                            p.db = path;
                            f.conn.url = crate::urlparts::build(&p);
                        }
                    }
                }
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

    fn pick_db_file(&self, ctx: &Context) {
        let tx = self.dialog.0.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            if let Some(f) = rfd::FileDialog::new().pick_file() {
                let _ = tx.send(Picked::DbFile(f));
                ctx.request_repaint();
            }
        });
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
        self.vars_applied = self.pass_edit.vars.clone();
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
        // El menú nativo se crea con los textos: se vuelve a crear.
        #[cfg(target_os = "macos")]
        {
            self.menu = crate::macmenu::MacMenu::new(ctx);
        }
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
            if ui.add(IconButton::new(Icon::Sidebar, Tone::Neutral).selected(self.show_sidebar)).on_hover_text(t!("Mostrar u ocultar la lista de conexiones (Ctrl+B)")).clicked() {
                self.show_sidebar = !self.show_sidebar;
            }
            if ui.add_enabled(self.form.is_some(), IconButton::new(Icon::Detail, Tone::Neutral).selected(self.show_form && self.form.is_some())).on_hover_text(t!("Mostrar u ocultar la ventana con los datos de la conexión (Ctrl+Mayús+B)")).clicked() {
                self.show_form = !self.show_form;
            }
            ui.separator();
            if ui.add(IconButton::new(Icon::New, Tone::File).label(t!("Nueva"))).on_hover_text(t!("Nueva conexión")).clicked() {
                self.new_connection();
            }
            let dirty = self.form.as_ref().is_some_and(|f| f.dirty() || f.force);
            if ui.add_enabled(dirty, IconButton::new(Icon::Save, Tone::File).label(t!("Guardar"))).on_hover_text("Ctrl+S").clicked() {
                self.save();
            }
            let has = self.form.is_some();
            if ui.add_enabled(has, IconButton::new(Icon::Copy, Tone::File).label(t!("Duplicar"))).clicked() {
                self.duplicate();
            }
            if ui.add_enabled(has, IconButton::new(Icon::Trash, Tone::File).label(t!("Borrar"))).clicked() {
                self.confirm_delete = true;
            }
            ui.separator();
            let busy = matches!(self.test, TestState::Running { .. });
            if ui.add_enabled(has && !busy, IconButton::new(Icon::Live, Tone::Live).label(t!("Probar conexión"))).on_hover_text("Ctrl+T").clicked() {
                self.test();
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(IconButton::new(Icon::Gear, Tone::Neutral).selected(self.show_settings)).on_hover_text(t!("Ajustes")).clicked() {
                    if self.show_settings {
                        self.show_settings = false;
                    } else {
                        self.open_settings();
                    }
                }
                if crate::update::SUPPORTED {
                    let new = self.updater.as_ref().map(|u| u.stage()).filter(|st| matches!(st, crate::update::Stage::Available(_) | crate::update::Stage::Done(_)));
                    // Como en buscador: ⬇ con un punto verde en la esquina si hay versión nueva.
                    let hint = match &new {
                        Some(crate::update::Stage::Available(v)) => tf!("Hay una versión nueva ({v}): clic para actualizar", v = v),
                        Some(crate::update::Stage::Done(_)) => t!("✔ Versión nueva instalada.").to_owned(),
                        _ => tf!("Buscar actualizaciones · versión {v}", v = crate::update::version_label()),
                    };
                    let r = ui.add(IconButton::new(Icon::Download, Tone::Live).selected(self.show_updater)).on_hover_text(hint);
                    if new.is_some() {
                        crate::icons::badge(ui, &r, Color32::from_rgb(40, 190, 80));
                    }
                    if r.clicked() {
                        update_clicked = true;
                    }
                }
                ui.separator();
                if ui.add(IconButton::new(Icon::Reload, Tone::Tool).label(t!("Reiniciar Java"))).on_hover_text(t!("Corta conexiones colgadas")).clicked() {
                    self.restart_java();
                }
                let dl = self.java_dl.as_ref().map(|s| s.lock().unwrap().clone());
                match dl {
                    Some(Dl::Running(m)) => {
                        ui.label(m);
                        ui.spinner();
                    }
                    Some(Dl::Failed(e)) => {
                        if ui.add(IconButton::new(Icon::Download, Tone::Live).label(t!("Reintentar"))).clicked() {
                            self.download_java(&ctx);
                        }
                        ui.colored_label(RED, e);
                    }
                    _ if !self.java_ok => {
                        if ui.add(IconButton::new(Icon::Download, Tone::Live).label(t!("Descargar Java portátil"))).on_hover_text(t!("JRE 21 de Eclipse Temurin (~50 MB) en la carpeta de configuración")).clicked() {
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
        let mut details = false;
        let new_unsaved = self.form.as_ref().filter(|f| f.mtime.is_none()).map(|f| f.conn.name.clone());
        ScrollArea::both().id_salt("lista_conexiones").auto_shrink(false).show(ui, |ui| {
            ui.style_mut().wrap_mode = Some(TextWrapMode::Extend);
            if let Some(n) = &new_unsaved {
                let _ = ui.selectable_label(true, RichText::new(format!("● {n}")).italics());
            }
            let mut group: Option<&str> = None;
            let mut toggle: Option<String> = None;
            for e in &self.entries {
                let c = &e.conn;
                if !q.is_empty() && ![&c.name, &c.group, &c.url].iter().any(|s| s.to_lowercase().contains(&q)) {
                    continue;
                }
                let folded = q.is_empty() && !c.group.is_empty() && self.folded_groups.contains(&c.group);
                if group != Some(c.group.as_str()) {
                    group = Some(c.group.as_str());
                    if !c.group.is_empty() {
                        ui.add_space(4.0);
                        // Clic en el grupo: plegar/desplegar sus conexiones.
                        let arrow = if folded { "⏵" } else { "⏷" };
                        let n = self.entries.iter().filter(|x| x.conn.group == c.group).count();
                        let text = if folded { format!("{arrow} 🗀 {}  ({n})", c.group) } else { format!("{arrow} 🗀 {}", c.group) };
                        if ui.add(Label::new(RichText::new(text).strong()).sense(Sense::click())).clicked() {
                            toggle = Some(c.group.clone());
                        }
                    }
                }
                if folded {
                    continue;
                }
                let sel = self.selected.as_deref() == Some(c.id.as_str()) && new_unsaved.is_none();
                let dirty = sel && self.form.as_ref().is_some_and(|f| f.dirty());
                let lock = if c.secret.is_empty() { "" } else { " 🔑" };
                let indent = if c.group.is_empty() { "" } else { "    " };
                let label = if dirty { format!("{indent}● {}{lock}", c.name) } else { format!("{indent}🗄 {}{lock}", c.name) };
                let r = ui.selectable_label(sel, label).on_hover_text(&c.url);
                // Clic: elegir y conectar (el explorador muestra sus tablas). Doble clic: ver sus datos.
                if r.clicked() {
                    clicked = Some(c.id.clone());
                }
                if r.double_clicked() {
                    details = true;
                }
            }
            if self.entries.is_empty() && new_unsaved.is_none() {
                ui.weak(t!("Sin conexiones. Pulsa «Nueva»."));
            }
            if let Some(g) = toggle {
                if !self.folded_groups.remove(&g) {
                    self.folded_groups.insert(g);
                }
            }
        });
        if let Some(id) = clicked {
            if self.selected.as_deref() != Some(id.as_str()) || new_unsaved.is_some() {
                self.select(id.clone());
                self.show_form = false;
            }
            let connected = self.console.session_conn == id && (self.console.session.is_some() || self.console.opening.is_some());
            if !connected && self.console.running.is_none() && self.form.as_ref().is_some_and(|f| f.conn.id == id) {
                self.connect();
            }
        }
        if details {
            self.show_form = true;
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
        let mut pick_db = false;
        let mut open_drivers = false;
        let mut want_driver = None;
        let catalog = self.catalog.dir.clone();
        let has_key = self.key.is_some();
        let driver_dl = self.driver_dl.as_ref().map(|(s, _)| s.lock().unwrap().clone());
        let Some(f) = &mut self.form else {
            ui.centered_and_justified(|ui| ui.weak(t!("Elige una conexión en el panel lateral para ver sus datos, o crea una con «Nueva».")));
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
            // Campos de la URL: editar uno rehace la URL; editar la URL los rellena.
            if let Some(mut p) = crate::urlparts::parse(&f.conn.url) {
                use crate::urlparts::Kind;
                let mut changed = false;
                if p.kind != Kind::File {
                    ui.label(t!("Servidor"));
                    ui.horizontal(|ui| {
                        changed |= ui.add(TextEdit::singleline(&mut p.host).hint_text("localhost").desired_width((w - 150.0).max(120.0))).changed();
                        ui.label(t!("Puerto"));
                        let def = crate::urlparts::default_port(&p);
                        let r = ui.add(TextEdit::singleline(&mut p.port).hint_text(def).desired_width(70.0));
                        if r.changed() {
                            changed = true;
                        }
                        r.on_hover_text(tf!("Vacío = {puerto} (el de siempre)", puerto = def));
                    });
                    ui.end_row();
                }
                let label = match p.kind {
                    Kind::OracleService => t!("Servicio"),
                    Kind::OracleSid => "SID",
                    Kind::File if p.prefix == "jdbc:sqlite:" => t!("Archivo"),
                    Kind::File => t!("Base (ruta o mem:nombre)"),
                    _ if p.prefix == "jdbc:firebirdsql:" => t!("Ruta de la base"),
                    _ => t!("Base de datos"),
                };
                ui.label(label);
                ui.horizontal(|ui| {
                    let extra = if p.kind == Kind::File || matches!(p.kind, Kind::OracleService | Kind::OracleSid) { 110.0 } else { 0.0 };
                    changed |= ui.add(TextEdit::singleline(&mut p.db).desired_width((w - extra).max(120.0))).changed();
                    match p.kind {
                        Kind::File => {
                            if ui.button(t!("Elegir…")).clicked() {
                                pick_db = true;
                            }
                        }
                        Kind::OracleService | Kind::OracleSid => {
                            let mut k = p.kind;
                            ComboBox::from_id_salt("oracle_forma").selected_text(if k == Kind::OracleSid { "SID" } else { t!("Servicio") }).width(90.0).show_ui(ui, |ui| {
                                ui.selectable_value(&mut k, Kind::OracleService, t!("Servicio"));
                                ui.selectable_value(&mut k, Kind::OracleSid, "SID");
                            });
                            if k != p.kind {
                                p.kind = k;
                                changed = true;
                            }
                        }
                        _ => {}
                    }
                });
                ui.end_row();
                if changed {
                    f.conn.url = crate::urlparts::build(&p);
                }
            } else if template_for(&f.conn.url).is_some() {
                ui.label("");
                ui.weak(t!("URL con formato especial: edítala directamente."));
                ui.end_row();
            }
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
                    if ui.add(IconButton::new(Icon::Open, Tone::File)).on_hover_text(t!("Abrir la carpeta de drivers descargados")).clicked() {
                        open_drivers = true;
                    }
                });
            });
            ui.end_row();
            // ¿Hay jar? Si no, ofrecer descargarlo de Maven Central.
            let jars = f.built().jar_paths(&catalog);
            let missing: Vec<_> = jars.iter().filter(|p| !p.is_file()).collect();
            // Sin jar propio se cargan todos los de drivers/: puede haber otros, pero no el de esta base.
            let template = template_for(&f.conn.url);
            let other = template.and_then(javadl::jar_hint).is_some_and(|h| {
                !jars.iter().any(|p| p.file_name().is_some_and(|n| n.to_string_lossy().to_lowercase().contains(h)))
            });
            if jars.is_empty() || !missing.is_empty() || other {
                ui.label("");
                ui.horizontal(|ui| {
                    let msg = if jars.is_empty() {
                        t!("⚠ No hay jar del driver.").to_owned()
                    } else if !missing.is_empty() {
                        tf!("⚠ No existe: {ruta}", ruta = missing[0].display())
                    } else {
                        tf!("⚠ No se ve el driver de {nombre} entre los jar.", nombre = template.unwrap_or_default())
                    };
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
                    if let Some(t) = template.filter(|t| javadl::maven_coords(t).is_some()) {
                        if !matches!(driver_dl, Some(Dl::Running(_))) && ui.add(IconButton::new(Icon::Download, Tone::Live).label(tf!("Descargar driver de {nombre}", nombre = t))).on_hover_text(t!("Desde Maven Central a la carpeta drivers del catálogo (se comparte con él)")).clicked() {
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
        if pick_db {
            self.pick_db_file(&ctx);
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
                    again = ui.add_enabled(self.form.is_some(), IconButton::new(Icon::Live, Tone::Live).label(t!("Probar otra vez"))).clicked();
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
            if self.session_id().is_some() && ui.add(IconButton::new(Icon::Reload, Tone::Neutral).small()).on_hover_text(t!("Recargar")).clicked() {
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
        let conn_name = self.console.session.as_ref().filter(|_| self.session_id().is_some()).map(|(_, n)| n.clone());
        // Nombres en una línea (sin ajustar); si no caben, se desplaza en horizontal.
        ScrollArea::both().id_salt("explorador").auto_shrink(false).show(ui, |ui| {
            ui.style_mut().wrap_mode = Some(TextWrapMode::Extend);
            let Some(conn_name) = conn_name else { return };
            let root = CollapsingHeader::new(RichText::new(format!("🗄 {conn_name}")).strong()).id_salt(("explorador_conexion", &conn_name)).default_open(true);
            root.show(ui, |ui| {
            for x in &mut self.explorer.schemas {
                let id = ui.make_persistent_id(("esquema", &x.cat, &x.sch));
                let header = CollapsingHeader::new(format!("🗀 {}", x.label())).id_salt(id).open(Some(x.open || (!q.is_empty() && x.tables.is_some())));
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
                // «Conectar» con la conexión elegida si no es la de la sesión; si ya lo es (o
                // está conectando), «Desconectar».
                let selected = self.form.as_ref().map(|f| (f.conn.id.clone(), f.conn.name.clone()));
                let connected_here = self.console.session.is_some() && selected.as_ref().is_none_or(|(id, _)| *id == self.console.session_conn);
                if connected_here || self.console.opening.is_some() {
                    if ui.button(t!("Desconectar")).clicked() {
                        self.disconnect();
                    }
                } else if let Some((_, name)) = selected {
                    if ui.add_enabled(self.console.running.is_none(), Button::new(t!("Conectar"))).on_hover_text(tf!("Abrir una sesión con {nombre}", nombre = name)).clicked() {
                        self.connect();
                    }
                }
                let can = self.form.is_some() || self.console.session.is_some();
                if self.console.running.is_some() {
                    if ui.add(IconButton::new(Icon::Stop, Tone::Edit).label(t!("Cancelar"))).clicked() {
                        self.cancel_sql();
                    }
                } else {
                    if ui.add_enabled(can, IconButton::new(Icon::FastForward, Tone::Live).label("Script")).on_hover_text(t!("Todas las sentencias, en orden (Ctrl+Mayús+Enter)")).clicked() {
                        run = Some(true);
                    }
                    if ui.add_enabled(can, IconButton::new(Icon::Live, Tone::Live).label(t!("Ejecutar"))).on_hover_text(t!("La selección o la sentencia del cursor (Ctrl+Enter)")).clicked() {
                        run = Some(false);
                    }
                }
                if ui.add(IconButton::new(Icon::Upload, Tone::File).label(t!("Importar CSV"))).clicked() {
                    self.open_import("");
                }
                if ui.add(IconButton::new(Icon::Clock, Tone::Tool).label(t!("Historial")).selected(self.console.show_history)).clicked() {
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
        // Alto del editor: se cambia arrastrando la barra de abajo.
        let max_h = (ui.available_height() - 120.0).max(80.0);
        let h = if self.console.editor_h <= 0.0 { 120.0 } else { self.console.editor_h }.clamp(60.0, max_h);
        let out = ScrollArea::vertical()
            .id_salt("sql_editor_scroll")
            .max_height(h)
            .min_scrolled_height(h)
            .auto_shrink([false, false])
            .show(ui, |ui| TextEdit::multiline(&mut self.console.sql).id(editor_id).code_editor().desired_rows(5).desired_width(f32::INFINITY).min_size(vec2(0.0, h)).layouter(&mut layouter).show(ui))
            .inner;
        let (grip, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 8.0), Sense::drag());
        let resp = resp.on_hover_cursor(CursorIcon::ResizeVertical).on_hover_text(t!("Arrastra para cambiar el alto del editor"));
        let col = if resp.hovered() || resp.dragged() { ui.visuals().widgets.hovered.fg_stroke.color } else { ui.visuals().widgets.noninteractive.bg_stroke.color };
        let c = grip.center();
        for dy in [-1.5, 1.5] {
            ui.painter().line_segment([pos2(c.x - 18.0, c.y + dy), pos2(c.x + 18.0, c.y + dy)], Stroke::new(1.0, col));
        }
        if resp.dragged() {
            self.console.editor_h = (h + resp.drag_delta().y).clamp(60.0, max_h);
        }
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
                        if ui.small_button("×").on_hover_text(t!("Cerrar esta pestaña")).clicked() {
                            close = Some(i);
                        }
                        ui.add_space(4.0);
                    }
                });
            });
        }
        if let Some(i) = close {
            let c = &mut self.console;
            if c.tabs.get(i).and_then(|t| t.edit.as_ref()).is_some_and(|e| !e.changes.is_empty()) {
                c.active = Some(i);
                self.status = t!("Esta pestaña tiene cambios sin guardar: aplícalos o descártalos antes de cerrarla.").into();
            } else if !(c.running.is_some() && c.current_tab && i + 1 == c.tabs.len()) {
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
                let mut copy_msg: Option<String> = None;
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
                            if ui.add(IconButton::new(Icon::Copy, Tone::File).label(t!("Copiar")).small()).clicked() {
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
                let (mut want_edit, mut want_apply, mut want_preview) = (false, false, false);
                let session_ok = self.session_id().is_some();
                let busy = self.console.running.is_some();
                if let Some(t) = self.console.tab() {
                    ui.horizontal(|ui| {
                        ui.weak(&t.info);
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.add(IconButton::new(Icon::Download, Tone::File).label(t!("Exportar…")).small()).clicked() {
                                want_export = true;
                            }
                            let label = if t.sel.is_empty() { t!("Copiar como TSV") } else { t!("Copiar selección") };
                            if !t.rows.is_empty() && ui.small_button(label).clicked() {
                                copy = true;
                            }
                            if t.edit.is_none() && !t.sql.is_empty() && !running {
                                if t.edit_req.is_some() {
                                    ui.spinner();
                                } else if let Some(why) = &t.read_only {
                                    ui.weak(t!("🔒 solo lectura")).on_hover_text(why);
                                } else if ui
                                    .add_enabled(session_ok, IconButton::new(Icon::Edit, Tone::Edit).label(t!("Editar")).small())
                                    .on_hover_text(t!("Cambiar valores de una o varias filas (si el resultado es de una sola tabla con clave primaria)"))
                                    .clicked()
                                {
                                    want_edit = true;
                                }
                            }
                        });
                    });
                    if let Some(e) = &mut t.edit {
                        ui.horizontal_wrapped(|ui| {
                            let n = e.changes.len();
                            ui.label(RichText::new(tf!("✎ {tabla}", tabla = e.sql_table)).strong());
                            if n == 0 {
                                ui.weak(t!("clic en una celda para editarla")).on_hover_text(t!("Clic en una celda para cambiarla; clic derecho: NULL, deshacer, copiar"));
                            } else {
                                ui.colored_label(ORANGE, tf!("{n} cambios en {filas} filas", n = n, filas = e.rows_changed()));
                            }
                            if e.applying.is_some() {
                                ui.spinner();
                            }
                            let can = n > 0 && e.applying.is_none();
                            if n > 0 && ui.add_enabled(can, IconButton::new(Icon::Eye, Tone::Tool).label(t!("Vista previa")).small()).on_hover_text(t!("Ver el SQL que se ejecutará")).clicked() {
                                want_preview = true;
                            }
                            if n > 0 && ui.add_enabled(can && !busy && session_ok, IconButton::new(Icon::Check, Tone::Live).label(t!("Aplicar")).small()).on_hover_text(t!("Guardar en la base: todo o nada")).clicked() {
                                want_apply = true;
                            }
                            if n > 0 && ui.add_enabled(can, Button::new(t!("Descartar")).small()).clicked() {
                                e.changes.clear();
                                e.editing = None;
                                e.error = None;
                            }
                        });
                        if let Some(err) = &e.error {
                            error_box(ui, err, 160.0);
                        }
                    }
                    if copy {
                        match selection_tsv(t, true) {
                            Some((text, msg)) => {
                                ui.ctx().copy_text(text);
                                copy_msg = Some(msg);
                            }
                            None => ui.ctx().copy_text(tsv(&t.cols, &t.rows)),
                        }
                    }
                    if let Some(msg) = results_table(ui, t, running) {
                        self.status = msg;
                    }
                }
                if copy {
                    self.status = copy_msg.unwrap_or_else(|| t!("Resultado copiado.").into());
                }
                if want_export {
                    self.open_export();
                }
                if want_edit {
                    self.start_edit();
                }
                if want_apply {
                    self.apply_edit();
                }
                if want_preview {
                    self.show_preview = true;
                }
            }
        }
    }

    /// Vista previa de los cambios de la pestaña activa (el SQL que se ejecutará).
    fn preview_window(&mut self, ctx: &Context) {
        let Some(sql) = self.console.tab().and_then(|t| t.edit.as_ref().filter(|e| !e.changes.is_empty()).map(|e| edit_preview(t, e))) else {
            self.show_preview = false;
            return;
        };
        let applying = self.console.tab().and_then(|t| t.edit.as_ref()).is_some_and(|e| e.applying.is_some());
        let can = self.session_id().is_some() && self.console.running.is_none() && !applying;
        let mut open = true;
        let mut apply = false;
        Window::new(t!("Vista previa de los cambios"))
            .id(win_id(Win::Preview))
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .min_size([360.0, 160.0])
            .default_size([680.0, 320.0])
            .default_pos(ctx.content_rect().center() - vec2(340.0, 160.0)).show(ctx, |ui| {
            ui.add(Label::new(RichText::new(t!("Al aplicar se ejecutan con parámetros y en una transacción: si un cambio falla o no encuentra su fila, no se guarda nada.")).weak()).wrap());
            // El SQL ocupa todo el alto de la ventana (menos la barra de botones) y se ajusta al ancho.
            let h = (ui.available_height() - 40.0).max(60.0);
            ScrollArea::vertical().id_salt("vista_previa").max_height(h).min_scrolled_height(h).auto_shrink([false, false]).show(ui, |ui| {
                ui.add(Label::new(highlight_job(&sql, ui.style(), None)).selectable(true).wrap());
            });
            ui.separator();
            ui.horizontal(|ui| {
                if ui.add_enabled(can, IconButton::new(Icon::Check, Tone::Live).label(t!("Aplicar"))).clicked() {
                    apply = true;
                }
                if applying {
                    ui.spinner();
                }
                if ui.add(IconButton::new(Icon::Copy, Tone::File).label(t!("Copiar"))).clicked() {
                    ui.ctx().copy_text(sql.clone());
                }
            });
        });
        if apply {
            self.apply_edit();
        }
        if !open {
            self.show_preview = false;
        }
    }

    /// Ventana para editar un valor de varias líneas de la tabla de resultados.
    fn big_edit_window(&mut self, ctx: &Context) {
        let Some(t) = self.console.tabs.iter_mut().find(|t| t.edit.as_ref().is_some_and(|e| e.big.is_some())) else { return };
        let e = t.edit.as_mut().unwrap();
        let (r, c, _) = e.big.as_ref().unwrap();
        let (r, c) = (*r, *c);
        let col = t.cols.get(c).map(|(n, ty)| if ty.is_empty() { n.clone() } else { format!("{n} ({ty})") }).unwrap_or_default();
        let original = t.rows.get(r).and_then(|row| row.get(c)).and_then(|v| v.as_deref()).map(str::to_owned);
        let mut open = true;
        // Some(Some(v)) = guardar v (None = NULL); Some(None) = cancelar.
        let mut done: Option<Option<Option<String>>> = None;
        Window::new(tf!("Editar valor · fila {n}", n = r + 1))
            .id(win_id(Win::BigEdit))
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .min_size([320.0, 180.0])
            .default_size([620.0, 380.0])
            .default_pos(ctx.content_rect().center() - vec2(310.0, 190.0))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.strong(&col);
                    if original.is_none() {
                        ui.weak(t!("(ahora es NULL)"));
                    }
                });
                let text = &mut e.big.as_mut().unwrap().2;
                let h = (ui.available_height() - 40.0).max(60.0);
                let edit_id = Id::unique("editar_valor_texto");
                ScrollArea::vertical().id_salt("editar_valor").max_height(h).min_scrolled_height(h).auto_shrink([false, false]).show(ui, |ui| {
                    let r = ui.add(TextEdit::multiline(text).id(edit_id).font(TextStyle::Monospace).desired_width(f32::INFINITY).min_size(vec2(0.0, h)).lock_focus(true));
                    if ui.memory(|m| m.focused().is_none()) {
                        r.request_focus();
                    }
                });
                let lines = text.lines().count().max(1);
                ui.horizontal(|ui| {
                    let accept = ui.add(IconButton::new(Icon::Check, Tone::Live).label(t!("Aceptar"))).on_hover_text("Ctrl+Enter").clicked() || ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Enter));
                    if accept {
                        done = Some(Some(Some(text.clone())));
                    }
                    if ui.button(t!("Poner NULL")).clicked() {
                        done = Some(Some(None));
                    }
                    if ui.button(t!("Cancelar")).clicked() {
                        done = Some(None);
                    }
                    ui.weak(tf!("{l} líneas · {c} caracteres", l = lines, c = text.chars().count()));
                });
            });
        if !open {
            done = Some(None);
        }
        match done {
            Some(Some(v)) => {
                if v.as_deref() == original.as_deref() {
                    e.changes.remove(&(r, c));
                } else {
                    e.changes.insert((r, c), v);
                }
                e.big = None;
            }
            Some(None) => e.big = None,
            None => {}
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
        let mut pick = false;
        let mut action: Option<u8> = None;
        let mut open_updater = false;
        let mut open_drivers = false;
        let mut rename_all = false;
        let mut want_java = false;
        let java_dl = self.java_dl.as_ref().map(|s| s.lock().unwrap().clone());
        let drivers = self.catalog.dir.join("drivers");
        let label = crate::update::version_label();
        let version = crate::estilo::Version { etiqueta: &label, actualizable: crate::update::SUPPORTED };
        let accion = crate::estilo::ventana_ajustes(ctx, &mut open, &version, |ui| {
            use crate::estilo::{fila, rejilla, seccion};
            {
                let s = &mut self.settings_edit;
                seccion(ui, t!("General"));
                rejilla(ui, "ajustes_general", |ui| {
                    fila(ui, t!("Idioma"), Some("Idioma · Language · Langue"), |ui| {
                        ui.radio_value(&mut s.lang, None, t!("Sistema"));
                        for l in Lang::ALL {
                            ui.radio_value(&mut s.lang, Some(l.code().to_owned()), l.name());
                        }
                    });
                    fila(ui, t!("Tema"), None, |ui| {
                        ui.radio_value(&mut s.dark, None, t!("Sistema"));
                        ui.radio_value(&mut s.dark, Some(false), t!("Claro"));
                        ui.radio_value(&mut s.dark, Some(true), t!("Oscuro"));
                    });
                    if crate::update::SUPPORTED {
                        fila(ui, t!("Actualizaciones"), None, |ui| {
                            ui.checkbox(&mut s.check_updates, t!("Avisar de versiones nuevas al arrancar"));
                        });
                    }
                    fila(ui, t!("Filas máximas"), Some(t!("Límite de filas por consulta")), |ui| {
                        ui.add(DragValue::new(&mut s.max_rows).range(1..=1_000_000));
                    });
                });
                seccion(ui, t!("Catálogo y Java"));
                rejilla(ui, "ajustes", |ui| {
                    ui.strong(t!("Catálogo")).on_hover_text(t!("Carpeta de las conexiones. Ponla en una carpeta compartida (red, git, Syncthing…) para usarlas desde otros equipos."));
                    ui.horizontal(|ui| {
                        let default = store::Settings::default().catalog_dir().display().to_string();
                        ui.add(TextEdit::singleline(&mut s.catalog).hint_text(default).desired_width(320.0));
                        if ui.button(t!("Elegir…")).clicked() {
                            pick = true;
                        }
                    });
                    ui.end_row();
                    let mismatched = self.entries.iter().filter(|e| e.conn.id != store::file_id(&e.conn.name)).count();
                    if mismatched > 0 {
                        ui.label("");
                        if ui.button(tf!("Renombrar {n} archivos .conexion como sus conexiones", n = mismatched)).on_hover_text(t!("Cada archivo pasa a llamarse como la conexión en la lista")).clicked() {
                            rename_all = true;
                        }
                        ui.end_row();
                    }
                    ui.strong(t!("Drivers"));
                    ui.horizontal(|ui| {
                        let path = drivers.display().to_string();
                        ui.allocate_ui(vec2(320.0, ui.spacing().interact_size.y), |ui| {
                            ui.add(Label::new(RichText::new(&path).weak()).truncate()).on_hover_text(&path);
                        });
                        if ui.add(IconButton::new(Icon::Open, Tone::File).label(t!("Abrir carpeta"))).on_hover_text(t!("Abrir la carpeta de drivers descargados")).clicked() {
                            open_drivers = true;
                        }
                    });
                    ui.end_row();
                    ui.strong("Java");
                    ui.add(TextEdit::singleline(&mut s.java).hint_text(t!("vacío = portátil, JAVA_HOME o el del PATH")).desired_width(320.0));
                    ui.end_row();
                    ui.strong(t!("Opciones JVM"));
                    ui.add(TextEdit::singleline(&mut s.jvm).hint_text("-Xmx384m").desired_width(320.0));
                    ui.end_row();
                });
                ui.weak(tf!("Java usado: {ruta}", ruta = bridge::java_path(&s.java).display()));
                // Java portátil: siempre se puede descargar (o renovar) desde aquí.
                ui.horizontal(|ui| {
                    match &java_dl {
                        Some(Dl::Running(msg)) => {
                            ui.spinner();
                            ui.label(msg.as_str());
                        }
                        other => {
                            let label = if javadl::installed().is_some() { t!("⬇ Volver a descargar Java portátil") } else { t!("⬇ Descargar Java portátil") };
                            if ui.button(label).on_hover_text(t!("JRE 21 de Eclipse Temurin (~50 MB) en la carpeta de configuración")).clicked() {
                                want_java = true;
                            }
                            match other {
                                Some(Dl::Failed(e)) => {
                                    ui.colored_label(RED, e.as_str());
                                }
                                _ => {
                                    if let Some(p) = javadl::installed() {
                                        ui.add(Label::new(RichText::new(tf!("portátil: {ruta}", ruta = p.display())).weak()).truncate());
                                    }
                                }
                            }
                        }
                    }
                });
            }
            let s = &mut self.settings_edit;
                seccion(ui, t!("Variables de este equipo"));
                ui.label(t!("NOMBRE=valor, una por línea. En el catálogo compartido usa ${VAR}, ${env:VAR} o ${VAR:-x} (x = valor por defecto) en URL, usuario, jar o propiedades: cada equipo pone sus valores."));
                ui.add(TextEdit::multiline(&mut self.pass_edit.vars).font(TextStyle::Monospace).hint_text("DB_HOST=localhost\nDB_PUERTO=5433").desired_rows(3).desired_width(f32::INFINITY));

                seccion(ui, t!("Frase común"));
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

                seccion(ui, t!("Servicio de red"));
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
        });
        match accion {
            crate::estilo::Accion::BuscarActualizaciones => open_updater = true,
            crate::estilo::Accion::Restablecer => {
                // Solo lo de presentación: catálogo, Java y servicio se conservan.
                let d = store::Settings { max_rows: 1000, check_updates: true, ..Default::default() };
                let s = &mut self.settings_edit;
                (s.dark, s.lang, s.max_rows, s.check_updates) = (d.dark, d.lang, d.max_rows, d.check_updates);
            }
            crate::estilo::Accion::Nada => {}
        }
        if open_updater {
            self.open_updater(ctx);
        }
        if open_drivers {
            self.open_drivers_dir();
        }
        if rename_all {
            self.rename_all_files();
        }
        if want_java {
            self.download_java(ctx);
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

    /// Bienvenida (común) con el catálogo vacío.
    fn welcome(&mut self, ui: &mut Ui) {
        let logo = crate::estilo::logo(ui.ctx(), &mut self.about_icon);
        let mut acciones = vec![
            (Icon::New, t!("Nueva conexión"), ""),
            (Icon::Gear, t!("Elegir la carpeta del catálogo…"), t!("Carpeta de las conexiones. Ponla en una carpeta compartida (red, git, Syncthing…) para usarlas desde otros equipos.")),
        ];
        if !self.java_ok {
            acciones.push((Icon::Cup, t!("Descargar Java portátil"), t!("JRE 21 de Eclipse Temurin (~50 MB) en la carpeta de configuración")));
        }
        let ctx = ui.ctx().clone();
        match crate::estilo::bienvenida(ui, &logo, &acciones, Some(t!("Las conexiones se guardan como archivos en el catálogo: compártelo para usarlas en otros equipos."))) {
            Some(0) => self.new_connection(),
            Some(1) => self.open_settings(),
            Some(2) => self.download_java(&ctx),
            _ => {}
        }
    }

    /// Ventana «Actualizar» (común a las tres aplicaciones: `estilo::ventana_actualizar`).
    fn updater_window(&mut self, ctx: &Context) {
        let Some(u) = &self.updater else { return };
        let mut open = true;
        match crate::estilo::ventana_actualizar(ctx, u, &mut open, None) {
            crate::estilo::AccionActualizar::Ejecutar => u.run(ctx.clone()),
            crate::estilo::AccionActualizar::Reiniciar(p) => match crate::update::relaunch_after_exit(&p) {
                Ok(()) => ctx.send_viewport_cmd(ViewportCommand::Close),
                Err(e) => self.status = tf!("No se pudo abrir la versión nueva: {e}", e = e),
            },
            crate::estilo::AccionActualizar::Nada => {}
        }
        if !open {
            self.show_updater = false;
        }
    }

    /// Recuerda dónde está la ventana. La primera vez que se sabe el monitor, si no
    /// es el de la última vez (se desconectó), la lleva a una esquina del principal.
    fn track_window(&mut self, ctx: &Context) {
        let (outer, inner, maximized, monitor) = ctx.input(|i| {
            let v = i.viewport();
            (v.outer_rect, v.inner_rect, v.maximized.unwrap_or(false), v.monitor_size)
        });
        let monitor = monitor.map(|m| (m.x, m.y));
        if !self.window_checked && monitor.is_some() {
            self.window_checked = true;
            if let (Some(old), Some(now)) = (self.saved_monitor, monitor) {
                if (old.0 - now.0).abs() > 1.0 || (old.1 - now.1).abs() > 1.0 {
                    ctx.send_viewport_cmd(ViewportCommand::OuterPosition(pos2(40.0, 40.0)));
                }
            }
        }
        let w = self.window.get_or_insert_with(Default::default);
        w.maximized = maximized;
        w.hide_sidebar = !self.show_sidebar;
        w.hide_form = !self.show_form;
        if !maximized {
            // Maximizada: se guarda el tamaño y sitio normales de antes.
            if let Some(r) = inner {
                w.size = (r.width(), r.height());
            }
            w.pos = outer.map(|r| (r.min.x, r.min.y));
            if monitor.is_some() {
                w.monitor = monitor;
            }
        } else if w.monitor.is_none() {
            w.monitor = monitor;
        }
        if w.size.0 < 200.0 {
            // Arrancó maximizada sin tamaño normal guardado: el actual sirve.
            if let Some(r) = inner {
                w.size = (r.width(), r.height());
            }
        }
    }

    /// Los Ajustes se aplican solos, sin botón: al cambiar una opción, o al terminar
    /// de escribir en un campo (o de arrastrar un número), para no reiniciar Java o el
    /// servicio con cada tecla. Al cerrar la ventana se aplica lo que quede.
    fn auto_apply_settings(&mut self, ctx: &Context) {
        let pending = self.settings_edit != self.settings || self.pass_edit.vars != self.vars_applied;
        if pending && (!self.show_settings || !(ctx.egui_wants_keyboard_input() || ctx.egui_is_using_pointer())) {
            self.apply_settings(ctx);
        }
    }

    fn open_settings(&mut self) {
        self.settings_edit = self.settings.clone();
        self.pass_edit.vars = store::vars_text();
        self.vars_applied = self.pass_edit.vars.clone();
        self.show_settings = true;
    }

    /// Opciones elegidas en el menú de macOS y marcas de «Ver».
    #[cfg(target_os = "macos")]
    fn mac_menu(&mut self, ctx: &Context) {
        use crate::macmenu::{Cmd, inject};
        for c in self.menu.poll() {
            match c {
                Cmd::About => self.show_about = true,
                Cmd::Update => self.open_updater(ctx),
                Cmd::Settings => self.open_settings(),
                Cmd::Url(u) => open_url(u),
                c => inject(ctx, c),
            }
        }
        self.menu.sync([self.show_sidebar, self.show_form && self.form.is_some()]);
    }

    /// «Acerca de» (ventana común a las tres aplicaciones: `estilo::acerca`),
    /// con Java y las carpetas de configuración y del catálogo.
    fn about_window(&mut self, ctx: &Context) {
        if !self.show_about {
            return;
        }
        let logo = crate::estilo::logo(ctx, &mut self.about_icon);
        let java = self.java.clone().unwrap_or_else(|| tf!("{ruta} (sin arrancar)", ruta = bridge::java_path(&self.settings.java).display()));
        let config = store::config_dir().map(|d| d.display().to_string()).unwrap_or_default();
        let catalog = self.catalog.dir.display().to_string();
        let label = crate::update::version_label();
        let version = crate::estilo::Version { etiqueta: &label, actualizable: crate::update::SUPPORTED };
        let accion = crate::estilo::acerca(ctx, &mut self.show_about, &logo, &version, |ui| {
            crate::estilo::fila_dato(ui, "Java", &java);
            crate::estilo::fila_carpeta(ui, t!("Configuración"), &config);
            crate::estilo::fila_carpeta(ui, t!("Catálogo"), &catalog);
        });
        if accion == crate::estilo::Accion::BuscarActualizaciones {
            self.open_updater(ctx);
        }
    }

    /// Ventana con los datos de la conexión elegida.
    fn form_window(&mut self, ctx: &Context) {
        let Some(f) = &self.form else { return };
        let name = f.conn.name.trim();
        let dirty = f.dirty();
        let title = format!("{}{}", if dirty { "● " } else { "" }, if name.is_empty() { t!("Conexión").to_owned() } else { tf!("Conexión: {nombre}", nombre = name) });
        let mut open = true;
        let max_h = (ctx.content_rect().height() - 80.0).max(200.0);
        Window::new(title)
            .id(win_id(Win::Form))
            .open(&mut open)
            .collapsible(true)
            .resizable(true)
            .default_size([760.0, 560.0])
            .default_pos(ctx.content_rect().center() - vec2(380.0, 300.0))
            .max_height(max_h)
            .show(ctx, |ui| {
                ScrollArea::vertical().id_salt("form_scroll").auto_shrink([false, true]).show(ui, |ui| self.form_ui(ui));
                ui.separator();
                ui.horizontal(|ui| {
                    let can_save = self.form.as_ref().is_some_and(|f| f.dirty() || f.force);
                    if ui.add_enabled(can_save, IconButton::new(Icon::Save, Tone::File).label(t!("Guardar"))).on_hover_text("Ctrl+S").clicked() {
                        self.save();
                    }
                    let busy = matches!(self.test, TestState::Running { .. });
                    if ui.add_enabled(!busy, IconButton::new(Icon::Live, Tone::Live).label(t!("Probar conexión"))).on_hover_text("Ctrl+T").clicked() {
                        self.test();
                    }
                    if ui.button(t!("Cerrar")).clicked() {
                        self.show_form = false;
                    }
                });
            });
        if !open {
            self.show_form = false;
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
            Win::Form => self.show_form && self.form.is_some(),
            Win::Preview => self.show_preview,
            Win::BigEdit => self.console.tabs.iter().any(|t| t.edit.as_ref().is_some_and(|e| e.big.is_some())),
            Win::Updater => self.show_updater && !self.updater.as_ref().is_some_and(|u| u.stage() == crate::update::Stage::Running),
            Win::Keys => self.show_keys,
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
            Win::Keys => self.show_keys = false,
            Win::Form => self.show_form = false,
            Win::Preview => self.show_preview = false,
            Win::BigEdit => {
                for t in &mut self.console.tabs {
                    if let Some(e) = &mut t.edit {
                        e.big = None;
                    }
                }
            }
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
        let open: Vec<Win> = [Win::Delete, Win::BigEdit, Win::Preview, Win::Keys, Win::Updater, Win::Test, Win::Export, Win::Import, Win::History, Win::Settings, Win::Form].into_iter().filter(|w| self.is_open(*w)).collect();
        let Some(first) = open.first().copied() else { return };
        let top = ctx.memory(|m| m.areas().top_layer_id(Order::Middle)).map(|l| l.id);
        let target = if self.confirm_delete { Win::Delete } else { open.iter().copied().find(|w| Some(win_id(*w)) == top).unwrap_or(first) };
        self.close(target);
        // Que el Escape no llegue además a otra cosa este fotograma.
        ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape));
    }

    /// Barra de estado común (atajos y «Acerca de» a la derecha).
    fn status_bar(&mut self, ui: &mut Ui) {
        let (mut keys, mut about) = (self.show_keys, self.show_about);
        crate::estilo::barra_estado(ui, &mut keys, &mut about, |ui| self.status_left(ui));
        (self.show_keys, self.show_about) = (keys, about);
    }

    fn status_left(&mut self, ui: &mut Ui) {
        let mut cancel_export = false;
        ui.horizontal(|ui| {
            let r = crate::estilo::etiqueta(ui, Icon::Open, RichText::new(self.catalog.dir.display().to_string()).small());
            if r.on_hover_text(t!("Catálogo (clic: copiar la ruta)")).clicked() {
                ui.ctx().copy_text(self.catalog.dir.display().to_string());
                self.avisos.info(t!("Ruta copiada"));
            }
            if self.key.is_some() {
                crate::estilo::etiqueta(ui, Icon::Lock, RichText::new(t!("desbloqueado")).small()).on_hover_text(t!("🔓 Desbloqueado en este equipo"));
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
                if ui.add(IconButton::new(Icon::Stop, Tone::Edit).small()).on_hover_text(t!("Cancelar")).clicked() {
                    cancel_export = true;
                }
            }
            if let Some(j) = &self.import_job {
                ui.separator();
                ui.spinner();
                ui.small(tf!("Importando… {n} filas · {s} s", n = j.done_rows, s = format!("{:.0}", j.started.elapsed().as_secs_f32())));
            }
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
            let want = std::env::var("CONECTOR_ELEGIR").unwrap_or_default();
            if let Some(id) = self.entries.iter().find(|e| e.conn.name.contains(&want)).map(|e| e.conn.id.clone()) {
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
            self.show_keys = std::env::var_os("CONECTOR_ATAJOS").is_some();
            if let Ok(m) = std::env::var("CONECTOR_AVISO") {
                self.status = m;
            }
            if std::env::var_os("CONECTOR_ACERCA").is_some() {
                self.show_about = true;
            }
            if std::env::var_os("CONECTOR_RENOMBRAR").is_some() {
                self.rename_all_files();
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
        // CONECTOR_ALTO_EDITOR=px fija el alto del editor SQL; CONECTOR_CELDA=fila,columna abre
        // la ventana de varias líneas de esa celda (con CONECTOR_VISTA=1, en su lugar, un cambio y la vista previa).
        if self.debug_frames == 3 {
            if let Some(h) = std::env::var("CONECTOR_ALTO_EDITOR").ok().and_then(|v| v.parse().ok()) {
                self.console.editor_h = h;
            }
        }
        if let Some((r, c)) = std::env::var("CONECTOR_CELDA").ok().and_then(|v| v.split_once(',').and_then(|(a, b)| Some((a.trim().parse::<usize>().ok()?, b.trim().parse::<usize>().ok()?)))) {
            let vista = std::env::var_os("CONECTOR_VISTA").is_some();
            let idle = self.console.running.is_none();
            if let Some(t) = self.console.tab().filter(|_| idle) {
                let cur = t.rows.get(r).and_then(|x| x.get(c)).and_then(|v| v.as_deref()).unwrap_or("").to_owned();
                if let Some(e) = t.edit.as_mut().filter(|e| e.big.is_none() && e.changes.is_empty()) {
                    CAPTURE_WAIT.store(40, std::sync::atomic::Ordering::Relaxed);
                    if !vista {
                        e.big = Some((r, c, format!("{cur}\nsegunda línea\ntercera línea")));
                    } else {
                        e.changes.insert((r, c), Some("Un texto bastante largo para comprobar que la vista previa ajusta las líneas al ancho de la ventana en vez de obligar a desplazarse".into()));
                        self.show_preview = true;
                    }
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
            || self.pending.values().any(|p| matches!(p, Pending::Meta(_) | Pending::Editable(_)))
            // Ventanas abiertas por la prueba: esperar a que terminen de aparecer.
            || CAPTURE_WAIT.fetch_update(std::sync::atomic::Ordering::Relaxed, std::sync::atomic::Ordering::Relaxed, |w| w.checked_sub(1)).is_ok();
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

/// Fotogramas que el modo captura espera antes de la foto (ventanas que aparecen).
static CAPTURE_WAIT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

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
    let bg = style.visuals.selection.bg_fill.gamma_multiply(if dark { 0.3 } else { 0.35 });
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

/// Abre un enlace o una carpeta con el programa del sistema (sin esperar).
#[cfg(target_os = "macos")]
fn open_url(url: &str) {
    let _ = open_in_file_manager(std::path::Path::new(url));
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
    Form,
    Preview,
    Keys,
    BigEdit,
}

fn win_id(w: Win) -> Id {
    match w {
        Win::Settings => return crate::estilo::id_ajustes(),
        Win::Updater => return crate::estilo::id_actualizar(),
        Win::Keys => return crate::estilo::id_atajos(),
        _ => {}
    }
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
                if ui.add(IconButton::new(Icon::Copy, Tone::File).label(t!("Copiar")).small()).on_hover_text(t!("Copia el error completo")).clicked() {
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
    // Fondo propio: algo más claro (tema claro) u oscuro (tema oscuro) que el panel.
    let v = ui.visuals();
    let res_bg = v.panel_fill.lerp_to_gamma(v.extreme_bg_color, 0.6);
    ui.painter().rect_filled(ui.available_rect_before_wrap(), 4.0, res_bg);
    ScrollArea::horizontal().id_salt("res_h").auto_shrink(false).show(ui, |ui| {
        ui.set_min_width(total_w);
        // Cabecera.
        let mods = ui.input(|i| i.modifiers);
        let sel_bg = ui.visuals().selection.bg_fill.gamma_multiply(0.45);
        let mut copy_menu: Option<bool> = None;
        let menu = |r: &Response, copy_menu: &mut Option<bool>| {
            r.context_menu(|ui| {
                if ui.button(t!("Copiar (con encabezados)")).clicked() {
                    *copy_menu = Some(true);
                    ui.close();
                }
                if ui.button(t!("Copiar sin encabezados")).clicked() {
                    *copy_menu = Some(false);
                    ui.close();
                }
            });
        };
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            let bg = ui.painter().add(Shape::Noop);
            let corner = ui.add_sized([48.0, row_h], Button::selectable(c.sel.all, RichText::new("#").weak()).frame_when_inactive(false));
            if corner.clicked() {
                let all = !c.sel.all;
                c.sel = TableSel { all, ..Default::default() };
            }
            menu(&corner.on_hover_text(t!("Seleccionar todo (Ctrl+C copia la selección)")), &mut copy_menu);
            let mut shapes = Vec::new();
            for (j, ((n, t), w)) in c.cols.iter().zip(&c.widths).enumerate() {
                let head = match &c.edit {
                    Some(e) if e.key.contains(&j) => RichText::new(format!("🔑 {n}")).strong().monospace(),
                    Some(e) if e.base.get(j).is_none_or(|b| b.is_none()) => RichText::new(n).monospace().color(weak),
                    _ => RichText::new(n).strong().monospace(),
                };
                let r = boxed(ui, *w, row_h, Label::new(head).truncate().selectable(false).sense(Sense::click()));
                if c.sel.col(j) && !c.sel.all {
                    shapes.push(Shape::rect_filled(r.rect, 0.0, sel_bg));
                }
                if r.clicked() {
                    c.sel.click(false, j, mods);
                }
                let r = if t.is_empty() { r } else { r.on_hover_text(t) };
                menu(&r, &mut copy_menu);
            }
            ui.painter().set(bg, Shape::Vec(shapes));
        });
        ui.separator();
        ScrollArea::vertical().id_salt("res_v").auto_shrink(false).show_rows(ui, row_h, c.rows.len(), |ui, range| {
            ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
            for i in range {
                let row = &c.rows[i];
                // Fondo de filas alternas: se reserva antes para quedar debajo del texto.
                let bg = ui.painter().add(Shape::Noop);
                let row_sel = c.sel.row(i);
                let mut cells = Vec::new();
                let r = ui.horizontal(|ui| {
                    let n = ui.add_sized([48.0, row_h], Label::new(RichText::new((i + 1).to_string()).color(weak).small()).selectable(false).sense(Sense::click()));
                    if n.clicked() {
                        c.sel.click(true, i, mods);
                    }
                    menu(&n, &mut copy_menu);
                    for (j, w) in c.widths.iter().enumerate() {
                        // Celda en edición: cuadro de texto (Enter o clic fuera guarda; Escape cancela).
                        if let Some(e) = c.edit.as_mut().filter(|e| e.editing.as_ref().is_some_and(|(r, cc, ..)| (*r, *cc) == (i, j))) {
                            let (_, _, text, focused) = e.editing.as_mut().unwrap();
                            let resp = boxed(ui, *w, row_h, TextEdit::singleline(text).font(TextStyle::Monospace).desired_width(*w - 10.0).margin(vec2(2.0, 0.0)));
                            if !*focused {
                                resp.request_focus();
                                *focused = true;
                            } else if resp.lost_focus() || !resp.has_focus() {
                                if ui.input(|i| i.key_pressed(Key::Escape)) {
                                    e.editing = None;
                                } else {
                                    e.commit_editing(&c.rows);
                                }
                            }
                            continue;
                        }
                        let (editable, changed) = match &c.edit {
                            Some(e) => (e.can_edit(row, j), e.changes.get(&(i, j)).cloned()),
                            None => (false, None),
                        };
                        let cell: Option<&str> = match &changed {
                            Some(v) => v.as_deref(),
                            None => row.get(j).and_then(|v| v.as_deref()),
                        };
                        let text = match cell {
                            Some(v) => RichText::new(v.replace(['\n', '\r', '\t'], " ")).monospace(),
                            None => RichText::new("NULL").italics().color(weak),
                        };
                        let r = boxed(ui, *w, row_h, Label::new(text).truncate().selectable(false).sense(Sense::click()));
                        if changed.is_some() {
                            cells.push(Shape::rect_filled(r.rect, 0.0, ORANGE.gamma_multiply(0.35)));
                        } else if !row_sel && c.sel.col(j) {
                            cells.push(Shape::rect_filled(r.rect, 0.0, sel_bg));
                        }
                        // Clic en una celda editable: se edita ahí mismo (como DBeaver); si no, se copia.
                        if editable && r.clicked() && mods.is_none() {
                            let current = cell.unwrap_or("").to_owned();
                            if let Some(e) = &mut c.edit {
                                e.commit_editing(&c.rows);
                                // Con saltos de línea, el cuadro de una línea los perdería: ventana.
                                if current.contains('\n') {
                                    e.big = Some((i, j, current));
                                } else {
                                    e.editing = Some((i, j, current, false));
                                }
                            }
                        } else if r.clicked() {
                            ui.ctx().copy_text(cell.unwrap_or("").to_owned());
                            copied = Some(tf!("Copiado: fila {n}, {columna}", n = i + 1, columna = c.cols[j].0));
                        }
                        if editable {
                            // 0 = editar, 1 = NULL, 2 = deshacer, 3 = copiar, 4 = editar en ventana
                            let mut act: Option<u8> = None;
                            let worthy = multiline_worthy(&c.cols[j].1, cell);
                            r.context_menu(|ui| {
                                if ui.button(t!("Editar")).clicked() {
                                    act = Some(0);
                                    ui.close();
                                }
                                if worthy && ui.button(t!("Editar en ventana (varias líneas)…")).clicked() {
                                    act = Some(4);
                                    ui.close();
                                }
                                if ui.button(t!("Copiar valor")).clicked() {
                                    act = Some(3);
                                    ui.close();
                                }
                                if ui.button(t!("Poner NULL")).clicked() {
                                    act = Some(1);
                                    ui.close();
                                }
                                if changed.is_some() && ui.button(t!("Deshacer este cambio")).clicked() {
                                    act = Some(2);
                                    ui.close();
                                }
                                if !c.sel.is_empty() {
                                    ui.separator();
                                    if ui.button(t!("Copiar (con encabezados)")).clicked() {
                                        copy_menu = Some(true);
                                        ui.close();
                                    }
                                    if ui.button(t!("Copiar sin encabezados")).clicked() {
                                        copy_menu = Some(false);
                                        ui.close();
                                    }
                                }
                            });
                            if act == Some(3) {
                                ui.ctx().copy_text(cell.unwrap_or("").to_owned());
                                copied = Some(tf!("Copiado: fila {n}, {columna}", n = i + 1, columna = c.cols[j].0));
                            } else if let (Some(a), Some(e)) = (act, &mut c.edit) {
                                match a {
                                    0 if cell.is_some_and(|v| v.contains('\n')) => e.big = Some((i, j, cell.unwrap_or("").to_owned())),
                                    0 => e.editing = Some((i, j, cell.unwrap_or("").to_owned(), false)),
                                    4 => {
                                        e.commit_editing(&c.rows);
                                        e.big = Some((i, j, cell.unwrap_or("").to_owned()));
                                    }
                                    1 if row.get(j).is_some_and(|v| v.is_none()) => {
                                        e.changes.remove(&(i, j));
                                    }
                                    1 => {
                                        e.changes.insert((i, j), None);
                                    }
                                    _ => {
                                        e.changes.remove(&(i, j));
                                    }
                                }
                            }
                        } else if !c.sel.is_empty() {
                            menu(&r, &mut copy_menu);
                        }
                    }
                });
                let mut shapes = Vec::new();
                if i % 2 == 1 {
                    shapes.push(Shape::rect_filled(r.response.rect, 0.0, stripe));
                }
                if row_sel {
                    shapes.push(Shape::rect_filled(r.response.rect, 0.0, sel_bg));
                }
                shapes.extend(cells);
                ui.painter().set(bg, Shape::Vec(shapes));
            }
        });
        // Ctrl+C (egui lo convierte en «Copy») copia la selección si no se está editando texto.
        let ctrl_c = ui.input(|i| i.events.iter().any(|e| matches!(e, eframe::egui::Event::Copy))) && ui.memory(|m| m.focused().is_none());
        let headers = copy_menu.or(if ctrl_c { Some(true) } else { None });
        if let Some((text, msg)) = headers.and_then(|h| selection_tsv(c, h)) {
            ui.ctx().copy_text(text);
            copied = Some(msg);
        }
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
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        crate::tunnel::close_all();
        if let Some(w) = self.window.as_ref().filter(|_| std::env::var_os("CONECTOR_CAPTURA").is_none() || std::env::var_os("CONECTOR_VENTANA").is_some()) {
            w.save();
        }
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
        #[cfg(target_os = "macos")]
        self.mac_menu(&ctx);
        // Con «Acerca de» abierto, Escape es para él (lo cierra el propio modal).
        if !self.show_about {
            self.escape_closes(&ctx);
        }
        self.track_window(&ctx);
        // Pruebas: CONECTOR_CERRAR_TRAS=N cierra la ventana (como el usuario) en el fotograma N.
        if let Some(n) = std::env::var("CONECTOR_CERRAR_TRAS").ok().and_then(|v| v.parse::<u64>().ok()) {
            if ctx.cumulative_frame_nr() >= n {
                ctx.send_viewport_cmd(ViewportCommand::Close);
            } else {
                ctx.request_repaint();
            }
        }
        let (settings_key, keys_key) = crate::estilo::atajos_comunes(&ctx);
        if settings_key {
            if self.show_settings {
                self.show_settings = false;
            } else {
                self.open_settings();
            }
        }
        if keys_key {
            self.show_keys = !self.show_keys;
        }
        // Los mensajes de estado se muestran como avisos flotantes (comunes a las tres apps).
        if self.status != self.status_seen {
            self.status_seen = self.status.clone();
            self.avisos.auto(self.status.clone());
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::S)) {
            self.save();
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::B)) {
            self.show_form = !self.show_form;
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::B)) {
            self.show_sidebar = !self.show_sidebar;
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::T)) && self.form.is_some() {
            self.test();
        }

        Panel::top("barra").show(ui, |ui| {
            ui.add_space(3.0);
            self.toolbar(ui);
            ui.add_space(1.0);
        });
        self.status_bar(ui);
        if self.show_sidebar {
            Panel::left("lista").resizable(true).default_size(230.0).min_size(150.0).show(ui, |ui| self.sidebar(ui));
        }
        // Explorador y consola ocupan la ventana; los datos de la conexión van en una ventana aparte.
        CentralPanel::default().show(ui, |ui| {
            if self.entries.is_empty() {
                self.welcome(ui);
            } else {
                self.console_ui(ui);
            }
        });
        if self.show_form && self.form.is_some() {
            self.form_window(&ctx);
        }
        if !matches!(self.test, TestState::Idle) {
            self.test_window(&ctx);
        }
        self.about_window(&ctx);
        if self.show_settings {
            self.settings_window(&ctx);
        }
        if self.show_preview {
            self.preview_window(&ctx);
        }
        self.big_edit_window(&ctx);
        self.auto_apply_settings(&ctx);
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
        let mac = |s: &str| crate::estilo::tecla(s);
        crate::estilo::ventana_atajos(
            &ctx,
            &mut self.show_keys,
            &[
                (
                    t!("Conexiones"),
                    vec![
                        (mac("Ctrl+S"), t!("Guardar la conexión")),
                        (mac("Ctrl+T"), t!("Probar la conexión")),
                        (mac("Ctrl+B"), t!("Mostrar u ocultar la lista de conexiones")),
                        (mac("Ctrl+Shift+B"), t!("Mostrar u ocultar los datos de la conexión")),
                    ],
                ),
                (
                    t!("Consola SQL"),
                    vec![
                        (mac("Ctrl+Enter"), t!("Ejecutar la selección o la sentencia del cursor")),
                        (mac("Ctrl+Shift+Enter"), t!("Ejecutar todo el script")),
                        (mac("Ctrl+C"), t!("Copiar las celdas elegidas (TSV)")),
                    ],
                ),
            ],
        );
        self.avisos.mostrar(&ctx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vista_previa_de_cambios() {
        assert_eq!(sql_literal(Some("42"), "INTEGER"), "42");
        assert_eq!(sql_literal(Some("4x"), "INTEGER"), "'4x'");
        assert_eq!(sql_literal(Some("O'Hara"), "VARCHAR"), "'O''Hara'");
        assert_eq!(sql_literal(None, "VARCHAR"), "NULL");
        assert_eq!(sql_literal(Some("true"), "BOOLEAN"), "true");
        let row = |v: &[Option<&str>]| v.iter().map(|x| x.map(Into::into)).collect::<Vec<Option<Box<str>>>>().into_boxed_slice();
        let t = ResultTab {
            cols: vec![("ID".into(), "INTEGER".into()), ("NOMBRE".into(), "VARCHAR".into()), ("EDAD".into(), "INTEGER".into())],
            rows: vec![row(&[Some("1"), Some("Ana"), Some("30")]), row(&[Some("2"), Some("Luis"), None])],
            ..Default::default()
        };
        let mut changes = std::collections::BTreeMap::new();
        changes.insert((0, 1), Some("Ana María".to_owned()));
        changes.insert((0, 2), None);
        changes.insert((1, 1), Some("Luis's".to_owned()));
        let e = EditState {
            conn: String::new(),
            cat: String::new(),
            sch: "PUBLIC".into(),
            table: "PERSONA".into(),
            sql_table: "PUBLIC.PERSONA".into(),
            base: vec![Some("ID".into()), Some("NOMBRE".into()), Some("EDAD".into())],
            sql_cols: vec!["ID".into(), "NOMBRE".into(), "EDAD".into()],
            key: vec![0],
            changes,
            editing: None,
            big: None,
            applying: None,
            error: None,
        };
        assert_eq!(
            edit_preview(&t, &e),
            "UPDATE PUBLIC.PERSONA SET NOMBRE = 'Ana María', EDAD = NULL WHERE ID = 1;\nUPDATE PUBLIC.PERSONA SET NOMBRE = 'Luis''s' WHERE ID = 2;\n"
        );
        assert_eq!(e.rows_changed(), 2);
        assert!(!e.can_edit(&t.rows[0], 0), "la clave no se edita");
        assert!(e.can_edit(&t.rows[0], 1));
    }
}

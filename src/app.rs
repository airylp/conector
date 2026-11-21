//! Interfaz: lista de conexiones (catálogo), formulario, prueba de conexión y
//! una consola SQL con resultados virtualizados (solo se dibujan las filas
//! visibles; las celdas se guardan como `Box<str>`).

use crate::bridge::{self, Bridge, Event};
use crate::proto::Msg;
use crate::store::{Catalog, Conn, Entry, SaveError, Secrets, Settings};
use eframe::egui::*;
use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

/// Plantillas de URL y clase de los drivers más comunes.
pub const TEMPLATES: &[(&str, &str, &str)] = &[
    ("PostgreSQL", "org.postgresql.Driver", "jdbc:postgresql://servidor:5432/basedatos"),
    ("MySQL", "com.mysql.cj.jdbc.Driver", "jdbc:mysql://servidor:3306/basedatos"),
    ("MariaDB", "org.mariadb.jdbc.Driver", "jdbc:mariadb://servidor:3306/basedatos"),
    ("Oracle", "oracle.jdbc.OracleDriver", "jdbc:oracle:thin:@//servidor:1521/servicio"),
    ("SQL Server", "com.microsoft.sqlserver.jdbc.SQLServerDriver", "jdbc:sqlserver://servidor:1433;databaseName=basedatos;encrypt=true;trustServerCertificate=true"),
    ("DB2", "com.ibm.db2.jcc.DB2Driver", "jdbc:db2://servidor:50000/basedatos"),
    ("Firebird", "org.firebirdsql.jdbc.FBDriver", "jdbc:firebirdsql://servidor:3050/ruta/base.fdb"),
    ("H2 (memoria)", "org.h2.Driver", "jdbc:h2:mem:prueba"),
    ("SQLite", "org.sqlite.JDBC", "jdbc:sqlite:/ruta/archivo.db"),
];

/// Cada cuánto se mira si otro equipo cambió el catálogo.
const SCAN_EVERY: Duration = Duration::from_secs(2);

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
                "causas" => detail.push_str(&format!("Causas:\n{v}\n")),
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

/// Formulario de la conexión seleccionada (textos tal cual se editan).
struct Form {
    conn: Conn,
    jars: String,
    props: String,
    password: String,
    remember: bool,
    /// Lo que hay guardado (para saber si hay cambios).
    saved: (Conn, String, bool),
    /// Fecha del archivo al leerlo; `None` = conexión nueva sin archivo.
    mtime: Option<Option<SystemTime>>,
    /// Otro equipo cambió el archivo mientras se editaba.
    external: bool,
    /// Siguiente «Guardar» sobrescribe aunque haya cambiado fuera.
    force: bool,
}

impl Form {
    fn from_entry(e: &Entry, password: Option<&str>) -> Form {
        let c = e.conn.clone();
        let pass = password.unwrap_or("").to_owned();
        let remember = password.is_some();
        Form {
            jars: c.jars.join("\n"),
            props: c.props.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("\n"),
            saved: (c.clone(), pass.clone(), remember),
            conn: c,
            password: pass,
            remember,
            mtime: Some(e.mtime),
            external: false,
            force: false,
        }
    }

    fn new_conn(name: String) -> Form {
        let c = Conn { name, ..Conn::new() };
        Form { jars: String::new(), props: String::new(), saved: (Conn::new(), String::new(), false), conn: c, password: String::new(), remember: false, mtime: None, external: false, force: false }
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
        self.mtime.is_none() || self.built() != self.saved.0 || self.remember != self.saved.2 || (self.remember && self.password != self.saved.1)
    }
}

#[derive(Default)]
struct Console {
    /// Sesión abierta en Java: (número, nombre de la conexión).
    session: Option<(String, String)>,
    opening: Option<u64>,
    /// SQL a ejecutar en cuanto se abra la sesión.
    queued: bool,
    sql: String,
    running: Option<(u64, Instant)>,
    cols: Vec<(String, String)>,
    rows: Vec<Box<[Option<Box<str>>]>>,
    widths: Vec<f32>,
    info: String,
    error: Option<ErrInfo>,
}

enum Pending {
    Test,
    Open,
    Query,
    Other,
}

enum Picked {
    Catalog(std::path::PathBuf),
    Jar(Vec<std::path::PathBuf>),
}

pub struct App {
    settings: Settings,
    catalog: Catalog,
    secrets: Secrets,
    entries: Vec<Entry>,
    signature: u64,
    last_scan: Instant,
    selected: Option<String>,
    form: Option<Form>,
    filter: String,
    bridge: Bridge,
    java: Option<String>,
    java_log: String,
    pending: HashMap<u64, Pending>,
    test: TestState,
    console: Console,
    status: String,
    show_settings: bool,
    settings_edit: Settings,
    confirm_delete: bool,
    dialog: (Sender<Picked>, Receiver<Picked>),
    debug_frames: u32,
}

fn new_bridge(ctx: &Context, s: &Settings) -> Bridge {
    let ctx = ctx.clone();
    Bridge::new(bridge::Config { java: s.java.clone(), jvm_opts: s.jvm.clone() }, Arc::new(move || ctx.request_repaint()))
}

impl App {
    pub fn new(cc: &eframe::CreationContext) -> App {
        let settings = Settings::load();
        match settings.dark {
            Some(true) => cc.egui_ctx.set_theme(ThemePreference::Dark),
            Some(false) => cc.egui_ctx.set_theme(ThemePreference::Light),
            None => {}
        }
        let catalog = Catalog { dir: settings.catalog_dir() };
        let _ = std::fs::create_dir_all(&catalog.dir);
        let entries = catalog.load();
        let mut app = App {
            bridge: new_bridge(&cc.egui_ctx, &settings),
            signature: catalog.signature(),
            settings_edit: settings.clone(),
            settings,
            catalog,
            secrets: Secrets::load(),
            entries,
            last_scan: Instant::now(),
            selected: None,
            form: None,
            filter: String::new(),
            java: None,
            java_log: String::new(),
            pending: HashMap::new(),
            test: TestState::Idle,
            console: Console { sql: "select 1".into(), ..Default::default() },
            status: String::new(),
            show_settings: false,
            confirm_delete: false,
            dialog: std::sync::mpsc::channel(),
            debug_frames: 0,
        };
        if let Some(id) = app.entries.first().map(|e| e.conn.id.clone()) {
            app.select(id);
        }
        app
    }

    fn secret_key(&self, id: &str) -> String {
        Secrets::key(&self.catalog.dir, id)
    }

    fn select(&mut self, id: String) {
        let Some(e) = self.entries.iter().find(|e| e.conn.id == id) else { return };
        let key = self.secret_key(&id);
        self.form = Some(Form::from_entry(e, self.secrets.get(&key)));
        self.selected = Some(id);
        self.test = TestState::Idle;
        self.confirm_delete = false;
    }

    fn reload(&mut self) {
        self.entries = self.catalog.load();
        self.signature = self.catalog.signature();
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
        let mut f = Form::new_conn("Nueva conexión".into());
        f.conn.id = self.catalog.free_id(&f.conn.name);
        self.selected = Some(f.conn.id.clone());
        self.form = Some(f);
        self.test = TestState::Idle;
    }

    fn duplicate(&mut self) {
        let Some(f) = &self.form else { return };
        let mut c = f.built();
        c.name = format!("{} (copia)", c.name);
        c.id = self.catalog.free_id(&c.name);
        let mut nf = Form::new_conn(c.name.clone());
        nf.jars = c.jars.join("\n");
        nf.props = f.props.clone();
        nf.conn = c;
        self.selected = Some(nf.conn.id.clone());
        self.form = Some(nf);
        self.test = TestState::Idle;
    }

    fn save(&mut self) {
        let Some(f) = &mut self.form else { return };
        let mut c = f.built();
        if c.name.trim().is_empty() {
            self.status = "Ponle un nombre a la conexión.".into();
            return;
        }
        c.name = c.name.trim().into();
        if f.mtime.is_none() && self.catalog.path_of(&c.id).exists() {
            c.id = self.catalog.free_id(&c.name);
        }
        let expected = if f.force { None } else { Some(f.mtime.unwrap_or(None)) };
        match self.catalog.save(&c, expected) {
            Ok(m) => {
                let key = Secrets::key(&self.catalog.dir, &c.id);
                let pass = f.remember.then(|| f.password.clone());
                if let Err(e) = self.secrets.set(key, pass) {
                    self.status = format!("Guardada, pero no la contraseña: {e}");
                } else {
                    self.status = format!("Guardada en {}", self.catalog.path_of(&c.id).display());
                }
                f.saved = (c.clone(), f.password.clone(), f.remember);
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
                self.status = "Otro equipo o programa cambió esta conexión. Pulsa Guardar otra vez para sobrescribirla.".into();
            }
            Err(SaveError::Io(e)) => self.status = format!("No se pudo guardar: {e}"),
        }
    }

    fn delete(&mut self) {
        let Some(id) = self.selected.clone() else { return };
        if self.form.as_ref().is_some_and(|f| f.mtime.is_some()) {
            if let Err(e) = self.catalog.delete(&id) {
                self.status = format!("No se pudo borrar: {e}");
                return;
            }
            let key = self.secret_key(&id);
            let _ = self.secrets.set(key, None);
        }
        self.form = None;
        self.selected = None;
        self.reload();
        self.status = "Conexión borrada.".into();
    }

    fn request_fields(&self, f: &Form) -> Vec<(String, String)> {
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
        let fields = self.request_fields(f);
        // Margen sobre el timeout del driver: la primera vez Java arranca (y compila).
        let limit = Duration::from_secs(f.conn.timeout.max(1) as u64 + if self.java.is_some() { 5 } else { 20 });
        let id = self.send("probar", &fields, Pending::Test);
        self.test = TestState::Running { id, since: Instant::now(), limit };
    }

    fn connect(&mut self) {
        let Some(f) = &self.form else { return };
        let fields = self.request_fields(f);
        let name = f.conn.name.clone();
        self.disconnect();
        let id = self.send("abrir", &fields, Pending::Open);
        self.console.opening = Some(id);
        self.console.session = Some((String::new(), name));
        self.console.error = None;
    }

    fn disconnect(&mut self) {
        if let Some((s, _)) = self.console.session.take().filter(|(s, _)| !s.is_empty()) {
            self.send("cerrar", &[("sesion".into(), s)], Pending::Other);
        }
        self.console.opening = None;
        self.console.queued = false;
    }

    fn run_sql(&mut self) {
        if self.console.running.is_some() {
            return;
        }
        let ready = self.console.session.as_ref().is_some_and(|(s, _)| !s.is_empty());
        if !ready {
            if self.console.opening.is_none() {
                if self.form.is_none() {
                    self.status = "Elige una conexión.".into();
                    return;
                }
                self.connect();
            }
            self.console.queued = true;
            return;
        }
        let s = self.console.session.as_ref().unwrap().0.clone();
        let sql = self.console.sql.trim().trim_end_matches(';').to_owned();
        let fields = [("sesion".to_owned(), s), ("sql".to_owned(), sql), ("max".to_owned(), self.settings.max_rows.to_string())];
        let id = self.send("consulta", &fields, Pending::Query);
        let c = &mut self.console;
        c.running = Some((id, Instant::now()));
        c.cols.clear();
        c.rows = Vec::new();
        c.widths.clear();
        c.info.clear();
        c.error = None;
    }

    fn cancel_sql(&mut self) {
        if let Some((id, _)) = self.console.running {
            self.send("cancelar", &[("objetivo".into(), id.to_string())], Pending::Other);
        }
    }

    fn restart_java(&mut self) {
        self.bridge.restart();
        self.status = "Java reiniciado: se cortan conexiones y consultas en curso.".into();
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
                    let detail = if log.is_empty() { String::new() } else { format!("Últimas líneas de Java:\n{log}") };
                    let ids: Vec<u64> = self.pending.keys().copied().collect();
                    for id in ids {
                        self.fail(id, ErrInfo { msg: "Java se cerró".into(), detail: detail.clone() });
                    }
                    if self.console.session.take().is_some() {
                        self.console.info = "Sesión cerrada (Java terminó).".into();
                    }
                    self.console.opening = None;
                }
            }
        }
    }

    fn fail(&mut self, id: u64, e: ErrInfo) {
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
                if self.console.running.is_some_and(|(r, _)| r == id) {
                    self.console.running = None;
                    self.console.error = Some(e);
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
                self.console.info = format!(
                    "Conectado en {} ms · {} {}",
                    m.get("ms").unwrap_or("?"),
                    m.get("producto").unwrap_or(""),
                    m.get("version").unwrap_or("")
                );
                if std::mem::take(&mut self.console.queued) {
                    self.run_sql();
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
                        c.cols = m
                            .fields
                            .iter()
                            .map(|f| {
                                let f = f.as_deref().unwrap_or("");
                                let (n, t) = f.split_once('\u{1}').unwrap_or((f, ""));
                                (n.to_owned(), t.to_owned())
                            })
                            .collect();
                    }
                    "fila" => c.rows.push(m.fields.into_boxed_slice()),
                    "fin" => {
                        self.pending.remove(&m.id);
                        let ms = m.get("ms").unwrap_or("?");
                        let upd = m.get("actualizadas").unwrap_or("");
                        c.info = if c.cols.is_empty() {
                            if upd.is_empty() { format!("Hecho en {ms} ms") } else { format!("Filas afectadas: {upd} · {ms} ms") }
                        } else {
                            let more = if m.get("mas") == Some("true") { " (hay más: sube el límite en Ajustes)" } else { "" };
                            format!("{} filas{more} · {ms} ms", c.rows.len())
                        };
                        c.running = None;
                        c.rows.shrink_to_fit();
                    }
                    _ => {}
                }
            }
            Some(Pending::Other) => {
                self.pending.remove(&m.id);
            }
            None => {}
        }
    }

    fn check_timeouts(&mut self, ctx: &Context) {
        if let TestState::Running { id, since, limit } = self.test {
            if since.elapsed() > limit {
                self.pending.remove(&id);
                self.test = TestState::Err(ErrInfo::plain(format!(
                    "Sin respuesta tras {} s. El driver puede seguir intentándolo; «Reiniciar Java» lo corta.",
                    limit.as_secs()
                )));
            } else {
                ctx.request_repaint_after(Duration::from_millis(200));
            }
        }
        if self.console.running.is_some() || self.console.opening.is_some() {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        if self.last_scan.elapsed() >= SCAN_EVERY {
            self.last_scan = Instant::now();
            if self.catalog.signature() != self.signature {
                self.reload();
            }
        }
        ctx.request_repaint_after(SCAN_EVERY);
        while let Ok(p) = self.dialog.1.try_recv() {
            match p {
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

    fn apply_settings(&mut self, ctx: &Context) {
        let s = self.settings_edit.clone();
        let java_changed = s.java != self.settings.java || s.jvm != self.settings.jvm;
        let catalog_changed = s.catalog_dir() != self.catalog.dir;
        if let Err(e) = s.save() {
            self.status = format!("No se pudieron guardar los ajustes: {e}");
        }
        match s.dark {
            Some(true) => ctx.set_theme(ThemePreference::Dark),
            Some(false) => ctx.set_theme(ThemePreference::Light),
            None => ctx.set_theme(ThemePreference::System),
        }
        self.settings = s;
        if java_changed {
            self.disconnect();
            self.bridge = new_bridge(ctx, &self.settings);
            self.java = None;
            self.pending.clear();
            self.test = TestState::Idle;
        }
        if catalog_changed {
            self.catalog = Catalog { dir: self.settings.catalog_dir() };
            let _ = std::fs::create_dir_all(&self.catalog.dir);
            self.form = None;
            self.selected = None;
            self.reload();
            if let Some(id) = self.entries.first().map(|e| e.conn.id.clone()) {
                self.select(id);
            }
        }
    }

    // ---------- dibujo ----------

    fn toolbar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            if ui.button("＋ Nueva").clicked() {
                self.new_connection();
            }
            let dirty = self.form.as_ref().is_some_and(|f| f.dirty() || f.force);
            if ui.add_enabled(dirty, Button::new("💾 Guardar")).on_hover_text("Ctrl+S").clicked() {
                self.save();
            }
            let has = self.form.is_some();
            if ui.add_enabled(has, Button::new("⧉ Duplicar")).clicked() {
                self.duplicate();
            }
            if ui.add_enabled(has, Button::new("🗑 Borrar")).clicked() {
                self.confirm_delete = true;
            }
            ui.separator();
            let busy = matches!(self.test, TestState::Running { .. });
            if ui.add_enabled(has && !busy, Button::new("▶ Probar conexión")).on_hover_text("Ctrl+T").clicked() {
                self.test();
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("⚙").on_hover_text("Ajustes").clicked() {
                    self.settings_edit = self.settings.clone();
                    self.show_settings = !self.show_settings;
                }
                if ui.button("⟳ Reiniciar Java").on_hover_text("Corta conexiones colgadas").clicked() {
                    self.restart_java();
                }
                let (color, text) = match self.java.as_ref().filter(|_| self.bridge.running()) {
                    Some(v) => (Color32::from_rgb(60, 170, 90), v.clone()),
                    None => (ui.visuals().weak_text_color(), "Java sin arrancar (arranca al probar)".into()),
                };
                let r = ui.label(RichText::new(format!("☕ {text}")).color(color));
                if !self.java_log.is_empty() {
                    r.on_hover_text(&self.java_log);
                }
            });
        });
    }

    fn sidebar(&mut self, ui: &mut Ui) {
        ui.add_space(4.0);
        ui.add(TextEdit::singleline(&mut self.filter).hint_text("🔎 Filtrar").desired_width(f32::INFINITY));
        ui.add_space(4.0);
        let q = self.filter.to_lowercase();
        let mut clicked = None;
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
                let label = if dirty { format!("● {}", c.name) } else { c.name.clone() };
                let r = ui.selectable_label(sel, label).on_hover_text(&c.url);
                if r.clicked() && !sel {
                    clicked = Some(c.id.clone());
                }
            }
            if self.entries.is_empty() && new_unsaved.is_none() {
                ui.weak("Sin conexiones. Pulsa «＋ Nueva».");
            }
        });
        if let Some(id) = clicked {
            self.select(id);
        }
    }

    fn form_ui(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        let mut pick_jar = false;
        let Some(f) = &mut self.form else {
            ui.centered_and_justified(|ui| ui.weak("Elige o crea una conexión."));
            return;
        };
        if f.external {
            ui.colored_label(Color32::from_rgb(220, 140, 30), "⚠ Otro equipo o programa cambió esta conexión mientras la editabas. Guardar la sobrescribe.");
        }
        // Ancho de los campos: lo que queda tras la columna de etiquetas (máx. 760).
        let w = (ui.available_width() - 140.0).clamp(200.0, 760.0);
        Grid::new("form").num_columns(2).spacing([10.0, 6.0]).min_col_width(120.0).show(ui, |ui| {
            ui.label("Nombre");
            ui.add(TextEdit::singleline(&mut f.conn.name).desired_width(w));
            ui.end_row();
            ui.label("Grupo");
            ui.add(TextEdit::singleline(&mut f.conn.group).hint_text("p. ej. Producción").desired_width(w));
            ui.end_row();
            ui.label("Tipo");
            ui.horizontal(|ui| {
                ComboBox::from_id_salt("plantilla").selected_text("Plantilla…").show_ui(ui, |ui| {
                    for (name, cls, url) in TEMPLATES {
                        if ui.selectable_label(false, *name).clicked() {
                            f.conn.url = (*url).into();
                            f.conn.driver = (*cls).into();
                        }
                    }
                });
                ui.weak("rellena URL y clase");
            });
            ui.end_row();
            ui.label("URL JDBC");
            ui.add(TextEdit::singleline(&mut f.conn.url).font(TextStyle::Monospace).hint_text("jdbc:…").desired_width(w));
            ui.end_row();
            ui.label("Usuario");
            ui.add(TextEdit::singleline(&mut f.conn.user).desired_width(w));
            ui.end_row();
            ui.label("Contraseña");
            ui.horizontal(|ui| {
                ui.add(TextEdit::singleline(&mut f.password).password(true).desired_width((w - 220.0).max(120.0)));
                ui.checkbox(&mut f.remember, "Recordar en este equipo").on_hover_text("Se guarda solo aquí (secretos.txt, permisos 600), nunca en el catálogo compartido.");
            });
            ui.end_row();
            ui.label("Clase del driver");
            ui.add(TextEdit::singleline(&mut f.conn.driver).font(TextStyle::Monospace).hint_text("vacío = la busca en los jar según la URL").desired_width(w));
            ui.end_row();
            ui.label("Jar del driver").on_hover_text("Uno por línea: archivos o carpetas, relativos al catálogo. Vacío = carpeta «drivers» del catálogo.");
            ui.horizontal_top(|ui| {
                ui.add(TextEdit::multiline(&mut f.jars).font(TextStyle::Monospace).hint_text("vacío = <catálogo>/drivers").desired_rows(2).desired_width((w - 90.0).max(120.0)));
                if ui.button("Añadir…").clicked() {
                    pick_jar = true;
                }
            });
            ui.end_row();
            ui.label("Propiedades");
            ui.add(TextEdit::multiline(&mut f.props).font(TextStyle::Monospace).hint_text("clave=valor, una por línea").desired_rows(2).desired_width(w));
            ui.end_row();
            ui.label("Espera (s)");
            ui.add(DragValue::new(&mut f.conn.timeout).range(1..=600));
            ui.end_row();
            ui.label("Notas");
            ui.add(TextEdit::multiline(&mut f.conn.notes).desired_rows(1).desired_width(w));
            ui.end_row();
        });
        if pick_jar {
            self.pick(&ctx, false);
        }
        ui.add_space(6.0);
        self.test_result(ui);
    }

    fn test_result(&mut self, ui: &mut Ui) {
        match &self.test {
            TestState::Idle => {}
            TestState::Running { since, .. } => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    let s = since.elapsed().as_secs_f32();
                    ui.label(if self.java.is_none() && s > 0.5 { format!("Arrancando Java y conectando… {s:.0} s") } else { format!("Conectando… {s:.1} s") });
                });
            }
            TestState::Ok(info) => {
                let get = |k: &str| info.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str()).unwrap_or("");
                ui.colored_label(Color32::from_rgb(60, 170, 90), RichText::new(format!("✔ Conectó en {} ms", get("ms"))).strong());
                Grid::new("info").num_columns(2).spacing([10.0, 2.0]).show(ui, |ui| {
                    for (k, label) in [
                        ("producto", "Base de datos"),
                        ("version", "Versión"),
                        ("driver", "Driver"),
                        ("driver_version", "Versión del driver"),
                        ("jdbc", "JDBC"),
                        ("usuario", "Usuario"),
                        ("catalogo", "Catálogo"),
                        ("esquema", "Esquema"),
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
            TestState::Err(e) => error_box(ui, e),
        }
    }

    fn console_ui(&mut self, ui: &mut Ui) {
        ui.set_min_height(ui.available_height());
        ui.add_space(4.0);
        let mut run = false;
        ui.horizontal(|ui| {
            ui.strong("Consola SQL");
            match (&self.console.session, self.console.opening) {
                (Some((_, n)), None) => {
                    ui.colored_label(Color32::from_rgb(60, 170, 90), format!("● {n}"));
                }
                (Some((_, n)), Some(_)) => {
                    ui.spinner();
                    ui.label(format!("Conectando a {n}…"));
                }
                _ => {
                    ui.weak("sin conectar (Ejecutar conecta con la seleccionada)");
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if self.console.session.is_some() && ui.button("Desconectar").clicked() {
                    self.disconnect();
                }
                if self.console.running.is_some() {
                    if ui.button("■ Cancelar").clicked() {
                        self.cancel_sql();
                    }
                } else if ui.add_enabled(self.form.is_some() || self.console.session.is_some(), Button::new("▶ Ejecutar")).on_hover_text("Ctrl+Enter").clicked() {
                    run = true;
                }
            });
        });
        let r = ui.add(TextEdit::multiline(&mut self.console.sql).code_editor().desired_rows(4).desired_width(f32::INFINITY));
        if r.has_focus() && ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Enter)) {
            run = true;
        }
        if run {
            self.run_sql();
        }
        ui.horizontal(|ui| {
            if let Some((_, t)) = self.console.running {
                ui.spinner();
                ui.label(format!("Ejecutando… {} filas · {:.1} s", self.console.rows.len(), t.elapsed().as_secs_f32()));
            } else {
                ui.weak(&self.console.info);
            }
            if !self.console.rows.is_empty() {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.small_button("Copiar como TSV").clicked() {
                        let t = tsv(&self.console.cols, &self.console.rows);
                        ui.ctx().copy_text(t);
                        self.status = "Resultado copiado.".into();
                    }
                });
            }
        });
        if let Some(e) = &self.console.error {
            error_box(ui, e);
        }
        if !self.console.cols.is_empty() {
            if let Some(msg) = results_table(ui, &mut self.console) {
                self.status = msg;
            }
        }
    }

    fn settings_window(&mut self, ctx: &Context) {
        let mut open = self.show_settings;
        let mut apply = false;
        let mut pick = false;
        Window::new("Ajustes").open(&mut open).collapsible(false).resizable(false).show(ctx, |ui| {
            let s = &mut self.settings_edit;
            Grid::new("ajustes").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Catálogo").on_hover_text("Carpeta de las conexiones. Ponla en una carpeta compartida (red, git, Syncthing…) para usarlas desde otros equipos.");
                ui.horizontal(|ui| {
                    ui.add(TextEdit::singleline(&mut s.catalog).hint_text(crate::store::Settings::default().catalog_dir().display().to_string()).desired_width(320.0));
                    if ui.button("Elegir…").clicked() {
                        pick = true;
                    }
                });
                ui.end_row();
                ui.label("Java");
                ui.add(TextEdit::singleline(&mut s.java).hint_text("vacío = JAVA_HOME o el del PATH").desired_width(320.0));
                ui.end_row();
                ui.label("Opciones JVM");
                ui.add(TextEdit::singleline(&mut s.jvm).hint_text("-Xmx384m").desired_width(320.0));
                ui.end_row();
                ui.label("Filas máximas");
                ui.add(DragValue::new(&mut s.max_rows).range(1..=1_000_000));
                ui.end_row();
                ui.label("Tema");
                ui.horizontal(|ui| {
                    ui.radio_value(&mut s.dark, None, "Sistema");
                    ui.radio_value(&mut s.dark, Some(false), "Claro");
                    ui.radio_value(&mut s.dark, Some(true), "Oscuro");
                });
                ui.end_row();
            });
            ui.add_space(6.0);
            ui.weak(format!("Java usado: {}", bridge::java_path(&s.java).display()));
            ui.add_space(6.0);
            if ui.button("Aplicar").clicked() {
                apply = true;
            }
        });
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
        Window::new("Borrar conexión").collapsible(false).resizable(false).anchor(Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
            ui.label(format!("¿Borrar «{name}» del catálogo? Desaparece también en los otros equipos que lo compartan."));
            ui.horizontal(|ui| {
                if ui.button("Borrar").clicked() {
                    delete = true;
                }
                if ui.button("Cancelar").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
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

    /// Pruebas: CONECTOR_CAPTURA=ruta.rgba guarda una captura (ancho, alto u32 LE +
    /// RGBA) y sale. CONECTOR_PROBAR=1 prueba la conexión seleccionada y
    /// CONECTOR_SQL=texto la ejecuta en la consola antes de capturar.
    fn debug_capture(&mut self, ctx: &Context) {
        let Ok(path) = std::env::var("CONECTOR_CAPTURA") else { return };
        self.debug_frames += 1;
        if self.debug_frames == 3 {
            if std::env::var_os("CONECTOR_PROBAR").is_some() {
                self.test();
            }
            if let Ok(sql) = std::env::var("CONECTOR_SQL") {
                self.console.sql = sql;
                self.run_sql();
            }
            if std::env::var_os("CONECTOR_AJUSTES").is_some() {
                self.show_settings = true;
            }
        }
        let busy = matches!(self.test, TestState::Running { .. }) || self.console.running.is_some() || self.console.opening.is_some() || self.console.queued;
        if self.debug_frames < 30 || (busy && self.debug_frames < 600) {
            ctx.request_repaint();
            return;
        }
        if self.debug_frames == u32::MAX / 2 {
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

fn error_box(ui: &mut Ui, e: &ErrInfo) {
    ui.colored_label(Color32::from_rgb(210, 70, 60), RichText::new(format!("✖ {}", e.msg)).strong());
    if !e.detail.is_empty() {
        CollapsingHeader::new("Detalle").id_salt(("detalle", &e.msg)).show(ui, |ui| {
            ui.add(Label::new(RichText::new(&e.detail).monospace().small()).wrap());
        });
    }
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
fn results_table(ui: &mut Ui, c: &mut Console) -> Option<String> {
    let font = TextStyle::Monospace.resolve(ui.style());
    let char_w = ui.fonts_mut(|f| f.glyph_width(&font, '0'));
    if c.widths.len() != c.cols.len() || (c.running.is_some() && c.rows.len() <= 200) {
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
                boxed(ui, *w, row_h, Label::new(RichText::new(n).strong().monospace()).truncate()).on_hover_text(t);
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
                            copied = Some(format!("Copiado: fila {}, {}", i + 1, c.cols[j].0));
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
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_bridge();
        self.check_timeouts(&ctx);
        self.debug_capture(&ctx);
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
        Panel::bottom("estado").show(ui, |ui| {
            ui.horizontal(|ui| {
                let r = ui.add(Label::new(RichText::new(format!("📁 {}", self.catalog.dir.display())).small()).sense(Sense::click()));
                if r.on_hover_text("Catálogo (clic: copiar la ruta)").clicked() {
                    ui.ctx().copy_text(self.catalog.dir.display().to_string());
                }
                ui.separator();
                ui.small(&self.status);
            });
        });
        Panel::left("lista").resizable(true).default_size(230.0).min_size(150.0).show(ui, |ui| self.sidebar(ui));
        Panel::bottom("consola").resizable(true).default_size(400.0).min_size(140.0).show(ui, |ui| self.console_ui(ui));
        CentralPanel::default().show(ui, |ui| {
            ScrollArea::vertical().id_salt("form_scroll").auto_shrink(false).show(ui, |ui| self.form_ui(ui));
        });
        if self.show_settings {
            self.settings_window(&ctx);
        }
        if self.confirm_delete {
            self.delete_window(&ctx);
        }
    }
}


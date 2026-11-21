// En Windows, sin ventana de consola junto a la app.
#![cfg_attr(windows, windows_subsystem = "windows")]
mod app;
mod bridge;
mod proto;
mod store;

use std::process::ExitCode;

const HELP: &str = "conector: prueba conexiones JDBC.

Uso:
  conector                   abre la interfaz
  conector --listar          conexiones del catálogo (id, nombre, URL)
  conector --probar ID       prueba una conexión (sale con 0 si conecta)
  conector --catalogo        ruta del catálogo

Variables: CONECTOR_CATALOGO (otra carpeta de catálogo), CONECTOR_CLAVE
(contraseña para --probar si no está recordada), JAVA_HOME.";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None => gui(),
        Some("--listar") => {
            let s = store::Settings::load();
            for e in (store::Catalog { dir: s.catalog_dir() }).load() {
                println!("{}\t{}\t{}", e.conn.id, e.conn.name, e.conn.url);
            }
            ExitCode::SUCCESS
        }
        Some("--catalogo") => {
            println!("{}", store::Settings::load().catalog_dir().display());
            ExitCode::SUCCESS
        }
        Some("--probar") => match args.get(1) {
            Some(id) => cli_test(id),
            None => {
                eprintln!("{HELP}");
                ExitCode::from(2)
            }
        },
        Some(_) => {
            eprintln!("{HELP}");
            ExitCode::from(2)
        }
    }
}

fn gui() -> ExitCode {
    let testing = std::env::var_os("CONECTOR_CAPTURA").is_some();
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_active(!testing)
            .with_title("Conector")
            .with_app_id("conector")
            .with_inner_size([1180.0, 820.0])
            .with_min_inner_size([760.0, 520.0]),
        ..Default::default()
    };
    match eframe::run_native("conector", options, Box::new(|cc| Ok(Box::new(app::App::new(cc))))) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("No se pudo abrir la ventana: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Prueba desde la línea de órdenes: útil para scripts y otros programas.
fn cli_test(id: &str) -> ExitCode {
    let s = store::Settings::load();
    let cat = store::Catalog { dir: s.catalog_dir() };
    let entries = cat.load();
    let Some(e) = entries.iter().find(|e| e.conn.id == id).or_else(|| entries.iter().find(|e| e.conn.name.eq_ignore_ascii_case(id))) else {
        eprintln!("No hay ninguna conexión «{id}» en {}", cat.dir.display());
        return ExitCode::from(2);
    };
    let secrets = store::Secrets::load();
    let pass = std::env::var("CONECTOR_CLAVE").ok().or_else(|| secrets.get(&store::Secrets::key(&cat.dir, &e.conn.id)).map(String::from)).unwrap_or_default();
    let fields = bridge::conn_fields(&e.conn, &pass, &cat.dir);
    let refs: Vec<(&str, &str)> = fields.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let mut b = bridge::Bridge::new(bridge::Config { java: s.java.clone(), jvm_opts: s.jvm.clone() }, std::sync::Arc::new(|| {}));
    let req = b.send("probar", &refs);
    let limit = std::time::Duration::from_secs(e.conn.timeout as u64 + 30);
    let start = std::time::Instant::now();
    loop {
        let left = limit.saturating_sub(start.elapsed());
        match b.rx.recv_timeout(left) {
            Ok(bridge::Event::Msg(m)) if m.id == req => {
                if &*m.kind == "ok" {
                    println!("OK\t{}", e.conn.name);
                    for (k, v) in m.pairs() {
                        println!("{k}\t{v}");
                    }
                    return ExitCode::SUCCESS;
                }
                println!("ERROR\t{}", e.conn.name);
                for (k, v) in m.pairs() {
                    println!("{k}\t{}", v.replace('\n', " | "));
                }
                return ExitCode::FAILURE;
            }
            Ok(bridge::Event::Failed(_, err)) => {
                println!("ERROR\t{}\nmensaje\t{err}", e.conn.name);
                return ExitCode::FAILURE;
            }
            Ok(bridge::Event::Exited(log)) => {
                println!("ERROR\t{}\nmensaje\tJava se cerró\n{log}", e.conn.name);
                return ExitCode::FAILURE;
            }
            Ok(_) => {}
            Err(_) => {
                println!("ERROR\t{}\nmensaje\tsin respuesta tras {} s", e.conn.name, limit.as_secs());
                return ExitCode::FAILURE;
            }
        }
    }
}

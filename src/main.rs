// En Windows, sin ventana de consola junto a la app.
#![cfg_attr(windows, windows_subsystem = "windows")]
// Primero: sus macros t!/tf! se usan en los demás módulos.
#[macro_use]
mod i18n;
mod app;
mod bridge;
mod crypto;
mod export;
mod icon;
mod javadl;
mod proto;
mod server;
mod sql;
mod store;
mod tls;

use std::process::ExitCode;

const HELP: &str = "conector: prueba conexiones JDBC.

Uso:
  conector                   abre la interfaz
  conector --listar          conexiones del catálogo (id, nombre, URL)
  conector --probar ID       prueba una conexión (sale con 0 si conecta)
  conector --catalogo        ruta del catálogo
  conector --frase           crea o desbloquea la frase común del catálogo en este
                             equipo (la lee de CONECTOR_FRASE o de la entrada)
  conector --clave ID        guarda la contraseña de ID compartida y cifrada
                             (de CONECTOR_CLAVE o de la entrada)
  conector --servir [DIR:PUERTO]
                             servicio HTTPS+JSON para otros programas (por defecto
                             el de Ajustes, 0.0.0.0:7470); muestra token y huella

Variables: CONECTOR_CATALOGO (otra carpeta de catálogo), CONECTOR_CLAVE
(contraseña para --probar si no está recordada), JAVA_HOME.";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None => gui(),
        // Para los instaladores: PNG del ícono (macOS/Linux) y .ico (Windows).
        Some("--iconset") => {
            let dir = std::path::PathBuf::from(args.get(1).map_or("conector.iconset", String::as_str));
            match icon::write_iconset(&dir) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("{e}");
                    ExitCode::FAILURE
                }
            }
        }
        Some("--ico") => match std::fs::write(args.get(1).map_or("conector.ico", String::as_str), icon::ico()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{e}");
                ExitCode::FAILURE
            }
        },
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
        Some("--servir") => serve(args.get(1).cloned()),
        Some("--frase") => passphrase(),
        Some("--clave") => match args.get(1) {
            Some(id) => shared_password(id),
            None => {
                eprintln!("{HELP}");
                ExitCode::from(2)
            }
        },
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
            .with_min_inner_size([760.0, 520.0])
            .with_icon(std::sync::Arc::new(icon::app_icon())),
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

/// Valor de una variable o, si no está, una línea de la entrada estándar.
fn secret_input(var: &str, prompt: &str) -> String {
    if let Ok(v) = std::env::var(var) {
        return v;
    }
    eprint!("{prompt}: ");
    let mut s = String::new();
    let _ = std::io::stdin().read_line(&mut s);
    s.trim_end_matches(['\n', '\r']).to_owned()
}

fn passphrase() -> ExitCode {
    let s = store::Settings::load();
    let dir = s.catalog_dir();
    let pass = secret_input("CONECTOR_FRASE", "Frase común");
    let mut secrets = store::Secrets::load();
    let key = match crypto::Lock::load(&dir) {
        Some(lock) => match lock.unlock(&pass) {
            Some(k) => k,
            None => {
                eprintln!("La frase no es correcta.");
                return ExitCode::FAILURE;
            }
        },
        None => match crypto::Lock::create(&pass) {
            Ok((lock, k)) => {
                if let Err(e) = lock.save(&dir) {
                    eprintln!("{e}");
                    return ExitCode::FAILURE;
                }
                println!("Frase creada en {}", dir.join(crypto::FILE).display());
                k
            }
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::FAILURE;
            }
        },
    };
    match secrets.set_catalog_key(&dir, Some(&key)) {
        Ok(()) => {
            println!("Este equipo recuerda la frase de {}", dir.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

fn shared_password(id: &str) -> ExitCode {
    let s = store::Settings::load();
    let cat = store::Catalog { dir: s.catalog_dir() };
    let Some(mut c) = cat.load().into_iter().map(|e| e.conn).find(|c| c.id == id) else {
        eprintln!("No hay ninguna conexión «{id}» en {}", cat.dir.display());
        return ExitCode::from(2);
    };
    let Some(key) = store::Secrets::load().catalog_key(&cat.dir).filter(|k| crypto::Lock::load(&cat.dir).is_some_and(|l| l.verify(k))) else {
        eprintln!("Este equipo no tiene la frase común: usa antes conector --frase");
        return ExitCode::FAILURE;
    };
    let pass = secret_input("CONECTOR_CLAVE", "Contraseña");
    c.secret = crypto::encrypt(&key, &pass);
    match cat.save(&c, None) {
        Ok(_) => {
            println!("Contraseña de «{}» guardada cifrada en el catálogo", c.name);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("No se pudo guardar: {e:?}");
            ExitCode::FAILURE
        }
    }
}

/// Servicio de red sin interfaz (para un servidor o un equipo compartido).
fn serve(addr: Option<String>) -> ExitCode {
    let mut s = store::Settings::load();
    if s.serve_token.trim().len() < 16 {
        s.serve_token = store::new_token();
        if let Err(e) = s.save() {
            eprintln!("No se pudo guardar el token: {e}");
        }
    }
    let tls = if s.serve_tls {
        match tls::load(&s.serve_cert, &s.serve_key) {
            Ok(t) => Some(t),
            Err(e) => {
                eprintln!("TLS: {e}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        None
    };
    let cfg = server::Config {
        tls: tls.as_ref().map(|t| t.config.clone()),
        addr: addr.unwrap_or_else(|| s.serve_addr.clone()),
        token: s.serve_token.clone(),
        share_passwords: s.serve_passwords,
        catalog: s.catalog_dir(),
        java: bridge::Config { java: s.java.clone(), jvm_opts: s.jvm.clone() },
        max_rows: s.max_rows,
    };
    match server::start(cfg.clone()) {
        Ok(srv) => {
            let scheme = if tls.is_some() { "https" } else { "http" };
            println!("Sirviendo {} en {scheme}://{}", cfg.catalog.display(), srv.addr);
            println!("Token: {}", cfg.token);
            match &tls {
                Some(t) => {
                    println!("Certificado: {}", t.cert_path.display());
                    println!("Huella SHA-256: {}", t.fingerprint);
                    println!(
                        "Prueba: curl --cacert '{}' -H 'Authorization: Bearer {}' https://localhost:{}/v1/conexiones",
                        t.cert_path.display(),
                        cfg.token,
                        srv.addr.rsplit(':').next().unwrap_or("7470")
                    );
                }
                None => {
                    println!("AVISO: sin TLS (servir_tls=no): todo viaja en claro.");
                    println!("Prueba: curl -H 'Authorization: Bearer {}' http://{}/v1/conexiones", cfg.token, srv.addr);
                }
            }
            loop {
                std::thread::park();
            }
        }
        Err(e) => {
            eprintln!("{e}");
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
    let pass = std::env::var("CONECTOR_CLAVE").ok().or_else(|| secrets.password_for(&e.conn, &cat.dir)).unwrap_or_default();
    let fields = match bridge::conn_fields(&e.conn, &pass, &cat.dir) {
        Ok(f) => f,
        Err(err) => {
            println!("ERROR\t{}\nmensaje\t{err}", e.conn.name);
            return ExitCode::FAILURE;
        }
    };
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

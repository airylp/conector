//! Menú nativo de macOS en la barra superior del sistema (como en buscador).
//!
//! En macOS el menú se queda con las combinaciones de teclas que muestra
//! (Cmd+C, Cmd+Z…), así que cada opción se convierte de vuelta en el mismo
//! evento de teclado que recibiría egui: cajas de texto y atajos funcionan
//! igual que sin menú.

use eframe::egui::{Context, Event, Key, Modifiers, ViewportCommand};
use muda::accelerator::Accelerator;
use muda::{CheckMenuItem, IsMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};
use std::sync::mpsc::{Receiver, channel};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Cmd {
    /// Repetir la combinación de teclas en egui.
    Key(Modifiers, Key),
    Copy,
    Cut,
    Paste,
    About,
    Update,
    Settings,
    /// Cerrar como con el botón de la ventana (guarda posición, cierra túneles).
    Quit,
    Url(&'static str),
}

pub struct MacMenu {
    _menu: Menu,
    rx: Receiver<MenuEvent>,
    items: Vec<(MenuId, Cmd)>,
    pub sidebar: CheckMenuItem,
    pub form: CheckMenuItem,
    /// Último estado aplicado (lista, datos).
    state: std::cell::Cell<Option<[bool; 2]>>,
}

impl MacMenu {
    pub fn new(ctx: &Context) -> Self {
        let (tx, rx) = channel();
        let c = ctx.clone();
        MenuEvent::set_event_handler(Some(move |e| {
            let _ = tx.send(e);
            c.request_repaint();
        }));

        let items = std::cell::RefCell::new(Vec::new());
        let accel = |a: &str| a.parse::<Accelerator>().ok();
        let item = |text: &str, a: &str, cmd: Cmd| {
            let it = MenuItem::new(text, true, accel(a));
            items.borrow_mut().push((it.id().clone(), cmd));
            it
        };
        let cmd = |k| Cmd::Key(Modifiers::COMMAND, k);
        let cmd_shift = |k| Cmd::Key(Modifiers::COMMAND | Modifiers::SHIFT, k);
        let sep = PredefinedMenuItem::separator;

        let about = item(t!("Acerca de conector"), "", Cmd::About);
        let update = item(t!("Buscar actualizaciones…"), "", Cmd::Update);
        let settings = item(t!("Ajustes…"), "Cmd+,", Cmd::Settings);
        let quit = item(t!("Salir de conector"), "Cmd+Q", Cmd::Quit);
        let app = Submenu::with_items(
            "conector",
            true,
            &[
                &about,
                &update,
                &sep(),
                &settings,
                &sep(),
                &PredefinedMenuItem::services(Some(t!("Servicios"))),
                &sep(),
                &PredefinedMenuItem::hide(Some(t!("Ocultar conector"))),
                &PredefinedMenuItem::hide_others(Some(t!("Ocultar otros"))),
                &PredefinedMenuItem::show_all(Some(t!("Mostrar todo"))),
                &sep(),
                &quit,
            ],
        )
        .unwrap();

        let save = item(t!("Guardar"), "Cmd+S", cmd(Key::S));
        let test = item(t!("Probar conexión"), "Cmd+T", cmd(Key::T));
        let file = Submenu::with_items(t!("Conexión"), true, &[&save, &test]).unwrap();

        let undo = item(t!("Deshacer"), "Cmd+Z", cmd(Key::Z));
        let redo = item(t!("Rehacer"), "Cmd+Shift+Z", cmd_shift(Key::Z));
        let cut = item(t!("Cortar"), "Cmd+X", Cmd::Cut);
        let copy = item(t!("Copiar"), "Cmd+C", Cmd::Copy);
        let paste = item(t!("Pegar"), "Cmd+V", Cmd::Paste);
        let all = item(t!("Seleccionar todo"), "Cmd+A", cmd(Key::A));
        let edit = Submenu::with_items(t!("Edición"), true, &[&undo, &redo, &sep(), &cut, &copy, &paste, &all]).unwrap();

        let sidebar = CheckMenuItem::new(t!("Lista de conexiones"), true, true, accel("Cmd+B"));
        items.borrow_mut().push((sidebar.id().clone(), cmd(Key::B)));
        let form = CheckMenuItem::new(t!("Datos de la conexión"), true, true, accel("Cmd+Shift+B"));
        items.borrow_mut().push((form.id().clone(), cmd_shift(Key::B)));
        let view = Submenu::with_items(t!("Ver"), true, &[&sidebar, &form, &sep(), &PredefinedMenuItem::fullscreen(Some(t!("Pantalla completa")))]).unwrap();

        let window = Submenu::with_items(
            t!("Ventana"),
            true,
            &[
                &PredefinedMenuItem::minimize(Some(t!("Minimizar"))),
                &PredefinedMenuItem::maximize(Some(t!("Zoom"))),
                &sep(),
                &PredefinedMenuItem::bring_all_to_front(Some(t!("Traer todo al frente"))),
            ],
        )
        .unwrap();
        let issue = item(t!("Reportar un problema"), "", Cmd::Url("https://github.com/airylp/conector/issues/new"));
        let source = item(t!("Código fuente"), "", Cmd::Url("https://github.com/airylp/conector"));
        let help = Submenu::with_items(t!("Ayuda"), true, &[&issue, &source]).unwrap();

        let menu = Menu::new();
        let subs: [&dyn IsMenuItem; 6] = [&app, &file, &edit, &view, &window, &help];
        menu.append_items(&subs).unwrap();
        menu.init_for_nsapp();
        window.set_as_windows_menu_for_nsapp();
        help.set_as_help_menu_for_nsapp();

        MacMenu { _menu: menu, rx, items: items.into_inner(), sidebar, form, state: Default::default() }
    }

    /// Refleja en el menú qué paneles se ven (sólo si cambió).
    pub fn sync(&self, st: [bool; 2]) {
        if self.state.get() == Some(st) {
            return;
        }
        self.state.set(Some(st));
        self.sidebar.set_checked(st[0]);
        self.form.set_checked(st[1]);
    }

    /// Opciones elegidas desde el último fotograma.
    pub fn poll(&self) -> Vec<Cmd> {
        self.rx.try_iter().filter_map(|e| self.items.iter().find(|(id, _)| *id == e.id).map(|(_, c)| *c)).collect()
    }
}

/// Mete el comando en la entrada de egui de este fotograma (los que son teclas o portapapeles).
pub fn inject(ctx: &Context, c: Cmd) {
    match c {
        Cmd::Key(modifiers, key) => ctx.input_mut(|i| {
            i.events.push(Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers });
        }),
        Cmd::Copy => ctx.input_mut(|i| i.events.push(Event::Copy)),
        Cmd::Cut => ctx.input_mut(|i| i.events.push(Event::Cut)),
        Cmd::Paste => ctx.send_viewport_cmd(ViewportCommand::RequestPaste),
        Cmd::Quit => ctx.send_viewport_cmd(ViewportCommand::Close),
        Cmd::About | Cmd::Update | Cmd::Settings | Cmd::Url(_) => {}
    }
}

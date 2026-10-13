//! Icône de one-mouse dans la zone de notification de Windows (en bas à droite).
//!
//! Fermer la fenêtre la cache au lieu de quitter : le partage continue, et un
//! clic sur l'icône la rouvre. Le clic droit propose « Ouvrir » et « Quitter ».
//! La fenêtre est montrée ou cachée directement par Windows : une fenêtre
//! cachée ne se redessine plus, egui ne pourrait pas la faire réapparaître.

pub use imp::{Tray, activate_existing};

#[cfg(windows)]
mod imp {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use eframe::egui;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{
        Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        FindWindowW, IsIconic, SW_HIDE, SW_RESTORE, SW_SHOW, SetForegroundWindow, ShowWindow,
    };

    pub struct Tray {
        _icon: TrayIcon,
        hwnd: isize,
        quit: Arc<AtomicBool>,
    }

    /// Fait réapparaître la fenêtre au premier plan.
    fn show(hwnd: isize, ctx: &egui::Context) {
        let hwnd = hwnd as *mut core::ffi::c_void;
        // SAFETY : HWND de la fenêtre principale, valide tant que l'application tourne.
        unsafe {
            ShowWindow(hwnd, SW_SHOW);
            if IsIconic(hwnd) != 0 {
                ShowWindow(hwnd, SW_RESTORE);
            }
            SetForegroundWindow(hwnd);
        }
        ctx.request_repaint();
    }

    /// Si one-mouse est déjà ouvert (même caché dans la zone de notification),
    /// remet sa fenêtre au premier plan et renvoie `true` : pas de second exemplaire.
    pub fn activate_existing() -> bool {
        let wide = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
        // Classe des fenêtres winit + titre : une fenêtre de l'explorateur sur un
        // dossier « one-mouse » ne compte pas.
        let (class, title) = (wide("Window Class"), wide("one-mouse"));
        // SAFETY : chaînes UTF-16 terminées par zéro, vivantes pendant l'appel.
        let hwnd = unsafe { FindWindowW(class.as_ptr(), title.as_ptr()) };
        if hwnd.is_null() {
            return false;
        }
        // SAFETY : fenêtre qui vient d'être trouvée.
        unsafe {
            ShowWindow(hwnd, SW_SHOW);
            if IsIconic(hwnd) != 0 {
                ShowWindow(hwnd, SW_RESTORE);
            }
            SetForegroundWindow(hwnd);
        }
        true
    }

    impl Tray {
        pub fn new(cc: &eframe::CreationContext<'_>, png: &[u8]) -> Option<Self> {
            let hwnd = match cc.window_handle().ok()?.as_raw() {
                RawWindowHandle::Win32(handle) => handle.hwnd.get(),
                _ => return None,
            };
            let image = image::load_from_memory(png)
                .ok()?
                .resize(32, 32, image::imageops::FilterType::Lanczos3)
                .to_rgba8();
            let (width, height) = image.dimensions();
            let icon = Icon::from_rgba(image.into_raw(), width, height).ok()?;

            let open = MenuItem::new("Ouvrir one-mouse", true, None);
            let quit_item = MenuItem::new("Quitter", true, None);
            let menu = Menu::new();
            menu.append(&open).ok()?;
            menu.append(&PredefinedMenuItem::separator()).ok()?;
            menu.append(&quit_item).ok()?;

            let tray = TrayIconBuilder::new()
                .with_icon(icon)
                .with_tooltip("one-mouse")
                .with_menu(Box::new(menu))
                .with_menu_on_left_click(false)
                .build()
                .map_err(|e| log::warn!("icône de la zone de notification : {e}"))
                .ok()?;

            let ctx = cc.egui_ctx.clone();
            TrayIconEvent::set_event_handler(Some(move |event| {
                if let TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                }
                | TrayIconEvent::DoubleClick {
                    button: MouseButton::Left,
                    ..
                } = event
                {
                    show(hwnd, &ctx);
                }
            }));

            let quit = Arc::new(AtomicBool::new(false));
            let ctx = cc.egui_ctx.clone();
            let (open_id, quit_id, flag) =
                (open.id().clone(), quit_item.id().clone(), quit.clone());
            MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
                if event.id == quit_id {
                    // La fenêtre doit être visible pour traiter la fermeture.
                    flag.store(true, Ordering::SeqCst);
                    show(hwnd, &ctx);
                } else if event.id == open_id {
                    show(hwnd, &ctx);
                }
            }));

            Some(Self {
                _icon: tray,
                hwnd,
                quit,
            })
        }

        /// Cache la fenêtre : il ne reste que l'icône en bas à droite.
        pub fn hide(&self) {
            // SAFETY : voir `show`.
            unsafe {
                ShowWindow(self.hwnd as *mut core::ffi::c_void, SW_HIDE);
            }
        }

        /// « Quitter » a été choisi dans le menu de l'icône.
        pub fn quit_requested(&self) -> bool {
            self.quit.load(Ordering::SeqCst)
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn activate_existing() -> bool {
        false
    }

    /// Pas d'icône hors Windows : fermer la fenêtre quitte one-mouse.
    pub enum Tray {}

    impl Tray {
        pub fn new(_cc: &eframe::CreationContext<'_>, _png: &[u8]) -> Option<Self> {
            None
        }

        pub fn hide(&self) {
            match *self {}
        }

        pub fn quit_requested(&self) -> bool {
            match *self {}
        }
    }
}

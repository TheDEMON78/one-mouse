//! Un seul curseur visible : celui du PC qu'on utilise.
//!
//! Le curseur est caché sur le PC qui contrôle l'autre (la souris est sur
//! l'autre écran), et sur le PC contrôlé une fois la souris revenue de l'autre
//! côté. Il réapparaît dès que le PC reprend la main, et toujours à la fin de
//! la connexion (y compris si one-mouse s'arrête brutalement : Windows rétabli
//! au démarrage suivant, X11 de lui-même).
//!
//! - Windows : curseurs du système remplacés par un curseur transparent, puis
//!   rétablis (`SPI_SETCURSORS`).
//! - Linux X11 : extension XFixes.
//! - Ailleurs (Wayland, macOS) : sans effet.

pub use imp::restore_at_startup;

/// Cache ou montre le curseur de ce PC ; le montre toujours en étant détruit.
pub struct CursorHider {
    hidden: bool,
    inner: imp::Hider,
}

impl CursorHider {
    pub fn new() -> Self {
        Self {
            hidden: false,
            inner: imp::Hider::new(),
        }
    }

    pub fn set_hidden(&mut self, hidden: bool) {
        if hidden != self.hidden {
            self.hidden = hidden;
            if hidden {
                self.inner.hide();
            } else {
                self.inner.show();
            }
        }
    }
}

impl Drop for CursorHider {
    fn drop(&mut self) {
        if self.hidden {
            self.inner.show();
        }
    }
}

#[cfg(windows)]
mod imp {
    use std::ptr::null_mut;

    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateCursor, SPI_SETCURSORS, SetSystemCursor, SystemParametersInfoW,
    };

    /// Curseurs standard de Windows (flèche, texte, main, sablier, redimensionnement…).
    const CURSORS: [u32; 15] = [
        32512, 32513, 32514, 32515, 32516, 32640, 32641, 32642, 32643, 32644, 32645, 32646, 32648,
        32649, 32650,
    ];

    pub struct Hider;

    impl Hider {
        pub fn new() -> Self {
            Self
        }

        pub fn hide(&self) {
            // Masques d'un curseur 32×32 transparent : ET à 1, OU exclusif à 0.
            let and = [0xFFu8; 128];
            let xor = [0u8; 128];
            for id in CURSORS {
                // SAFETY : masques de la taille annoncée ; `SetSystemCursor` prend
                // possession du curseur créé.
                unsafe {
                    let blank = CreateCursor(
                        null_mut(),
                        0,
                        0,
                        32,
                        32,
                        and.as_ptr().cast(),
                        xor.as_ptr().cast(),
                    );
                    if !blank.is_null() {
                        SetSystemCursor(blank, id);
                    }
                }
            }
        }

        pub fn show(&self) {
            restore_at_startup();
        }
    }

    /// Rétablit les curseurs de l'utilisateur (après un arrêt brutal de one-mouse).
    pub fn restore_at_startup() {
        // SAFETY : recharge les curseurs du thème de l'utilisateur.
        unsafe {
            SystemParametersInfoW(SPI_SETCURSORS, 0, null_mut(), 0);
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::ptr::null;

    use x11::xfixes::{XFixesHideCursor, XFixesShowCursor};
    use x11::xlib::{XCloseDisplay, XDefaultRootWindow, XFlush, XOpenDisplay};

    /// Connexion X11 dédiée : si one-mouse s'arrête, le serveur X la ferme et
    /// montre de nouveau le curseur de lui-même.
    pub struct Hider {
        display: *mut x11::xlib::Display,
    }

    impl Hider {
        pub fn new() -> Self {
            // SAFETY : `XOpenDisplay(NULL)` renvoie NULL sans X11 (Wayland pur…).
            let display = unsafe { XOpenDisplay(null()) };
            Self { display }
        }

        pub fn hide(&self) {
            if self.display.is_null() {
                return;
            }
            // SAFETY : connexion ouverte par `new`.
            unsafe {
                XFixesHideCursor(self.display, XDefaultRootWindow(self.display));
                XFlush(self.display);
            }
        }

        pub fn show(&self) {
            if self.display.is_null() {
                return;
            }
            // SAFETY : connexion ouverte par `new`.
            unsafe {
                XFixesShowCursor(self.display, XDefaultRootWindow(self.display));
                XFlush(self.display);
            }
        }
    }

    impl Drop for Hider {
        fn drop(&mut self) {
            if !self.display.is_null() {
                // SAFETY : fermée une seule fois.
                unsafe {
                    XCloseDisplay(self.display);
                }
            }
        }
    }

    pub fn restore_at_startup() {}
}

#[cfg(not(any(windows, target_os = "linux")))]
mod imp {
    pub struct Hider;

    impl Hider {
        pub fn new() -> Self {
            Self
        }

        pub fn hide(&self) {}

        pub fn show(&self) {}
    }

    pub fn restore_at_startup() {}
}

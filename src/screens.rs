//! Écrans des deux PC et alignement du curseur entre eux.
//!
//! Chaque PC décrit ses écrans (position et taille dans son bureau) à l'autre.
//! Quand le curseur passe d'un PC à l'autre, il arrive à la même hauteur (ou à
//! la même position horizontale si l'autre PC est au-dessus ou en dessous),
//! compte tenu du décalage entre les deux bureaux réglé dans l'onglet Écrans.
//!
//! - Windows : `EnumDisplayMonitors`, `GetCursorPos`, `SetCursorPos` (en pixels
//!   réels de chaque écran, comme la capture).
//! - Linux X11 : XRandR et `XWarpPointer`.
//! - Ailleurs (Wayland, macOS) : écrans inconnus, le curseur garde sa position.

use input_capture::Position;
use serde::{Deserialize, Serialize};

/// Un écran, en pixels, dans les coordonnées du bureau de son PC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Monitor {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Monitor {
    fn right(&self) -> i32 {
        self.x + self.width
    }

    fn bottom(&self) -> i32 {
        self.y + self.height
    }
}

/// Rectangle englobant tous les écrans.
pub fn bounds(monitors: &[Monitor]) -> Option<Monitor> {
    let x = monitors.iter().map(|m| m.x).min()?;
    let y = monitors.iter().map(|m| m.y).min()?;
    let right = monitors.iter().map(Monitor::right).max()?;
    let bottom = monitors.iter().map(Monitor::bottom).max()?;
    Some(Monitor {
        x,
        y,
        width: right - x,
        height: bottom - y,
    })
}

/// Le long du bord partagé avec l'autre PC : axe vertical si l'autre PC est à
/// gauche ou à droite, horizontal sinon.
fn vertical(side: Position) -> bool {
    matches!(side, Position::Left | Position::Right)
}

/// Position du point le long du bord partagé, comptée depuis le début du
/// bureau (haut ou gauche) : ce qui est envoyé à l'autre PC.
pub fn along(monitors: &[Monitor], side: Position, point: (i32, i32)) -> Option<i32> {
    let b = bounds(monitors)?;
    Some(if vertical(side) {
        point.1 - b.y
    } else {
        point.0 - b.x
    })
}

/// Point d'arrivée du curseur venant de l'autre PC, situé du côté `side` :
/// `along` (compté depuis le début du bureau de l'autre PC) décalé de `offset`
/// (début du bureau de l'autre PC par rapport à celui-ci), ramené sur l'écran
/// de ce bord le plus proche, juste à l'intérieur.
pub fn entry_point(
    monitors: &[Monitor],
    side: Position,
    along: i32,
    offset: i32,
) -> Option<(i32, i32)> {
    let b = bounds(monitors)?;
    let target = along + offset + if vertical(side) { b.y } else { b.x };
    // Seuls les écrans posés sur le bord du bureau (le plus à gauche, à
    // droite…) : le bord du retour y est atteignable. Parmi eux, celui qui
    // couvre la position visée, sinon le plus proche.
    let span = |m: &Monitor| {
        if vertical(side) {
            (m.y, m.bottom())
        } else {
            (m.x, m.right())
        }
    };
    let distance = |m: &Monitor| {
        let (start, end) = span(m);
        if target < start {
            start - target
        } else if target >= end {
            target - end + 1
        } else {
            0
        }
    };
    let depth = |m: &Monitor| match side {
        Position::Left => m.x,
        Position::Right => -m.right(),
        Position::Top => m.y,
        Position::Bottom => -m.bottom(),
    };
    let edge = monitors.iter().map(depth).min()?;
    let monitor = monitors
        .iter()
        .filter(|m| depth(m) == edge)
        .min_by_key(|m| distance(m))?;
    let (start, end) = span(monitor);
    let along = target.clamp(start, end - 1);
    // Un peu à l'intérieur : le bord du retour n'est pas touché tout de suite.
    const INSET: i32 = 2;
    Some(match side {
        Position::Left => (monitor.x + INSET, along),
        Position::Right => (monitor.right() - 1 - INSET, along),
        Position::Top => (along, monitor.y + INSET),
        Position::Bottom => (along, monitor.bottom() - 1 - INSET),
    })
}

/// Écrans de ce PC (vide si inconnus).
pub fn local_monitors() -> Vec<Monitor> {
    imp::monitors()
}

/// Position actuelle du curseur de ce PC.
pub fn cursor_position() -> Option<(i32, i32)> {
    imp::cursor_position()
}

/// Place le curseur de ce PC ; `false` si ce n'est pas possible ici.
pub fn warp_cursor(point: (i32, i32)) -> bool {
    imp::warp(point)
}

/// Bouton principal de la souris enfoncé (glisser-déposer en cours…).
pub fn primary_button_down() -> bool {
    imp::primary_button_down()
}

/// Windows : coordonnées en pixels réels de chaque écran, comme la capture.
/// À appeler au démarrage du moteur, avant toute fenêtre.
pub fn enable_dpi_awareness() {
    #[cfg(windows)]
    // SAFETY : réglage du processus, sans pointeur.
    unsafe {
        use windows_sys::Win32::UI::HiDpi::{
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
        };
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

#[cfg(windows)]
mod imp {
    use std::ptr::null;

    use windows_sys::Win32::Foundation::{LPARAM, POINT, RECT};
    use windows_sys::Win32::Graphics::Gdi::{EnumDisplayMonitors, HDC, HMONITOR};
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetCursorPos, SetCursorPos};

    use super::Monitor;

    unsafe extern "system" fn collect(
        _monitor: HMONITOR,
        _hdc: HDC,
        rect: *mut RECT,
        data: LPARAM,
    ) -> windows_sys::core::BOOL {
        // SAFETY : `data` pointe sur le vecteur de `monitors`, `rect` sur le
        // rectangle de l'écran, valides pendant l'appel.
        unsafe {
            let monitors = &mut *(data as *mut Vec<Monitor>);
            let r = &*rect;
            monitors.push(Monitor {
                x: r.left,
                y: r.top,
                width: r.right - r.left,
                height: r.bottom - r.top,
            });
        }
        1
    }

    pub fn monitors() -> Vec<Monitor> {
        let mut monitors: Vec<Monitor> = Vec::new();
        // SAFETY : le vecteur vit pendant tout l'appel, qui est synchrone.
        unsafe {
            EnumDisplayMonitors(
                std::ptr::null_mut(),
                null(),
                Some(collect),
                &mut monitors as *mut Vec<Monitor> as LPARAM,
            );
        }
        monitors
    }

    pub fn cursor_position() -> Option<(i32, i32)> {
        let mut point = POINT { x: 0, y: 0 };
        // SAFETY : pointeur vers une variable locale.
        (unsafe { GetCursorPos(&mut point) } != 0).then_some((point.x, point.y))
    }

    pub fn warp(point: (i32, i32)) -> bool {
        // SAFETY : sans pointeur.
        unsafe { SetCursorPos(point.0, point.1) != 0 }
    }

    pub fn primary_button_down() -> bool {
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
            GetAsyncKeyState, VK_LBUTTON, VK_RBUTTON,
        };
        use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_SWAPBUTTON};
        // SAFETY : fonctions sans pointeur. Boutons inversés : le principal est
        // le bouton physique de droite.
        unsafe {
            let key = if GetSystemMetrics(SM_SWAPBUTTON) != 0 {
                VK_RBUTTON
            } else {
                VK_LBUTTON
            };
            (GetAsyncKeyState(key as i32) as u16 & 0x8000) != 0
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::os::raw::c_uint;
    use std::ptr::null;

    use x11::xlib;

    use super::Monitor;

    /// Wayland : le curseur réel n'est pas celui de XWayland, on ne le déplace pas.
    fn wayland() -> bool {
        std::env::var_os("WAYLAND_DISPLAY").is_some()
            || std::env::var("XDG_SESSION_TYPE").is_ok_and(|t| t == "wayland")
    }

    /// Connexion X11 le temps d'un appel.
    fn with_display<T>(f: impl FnOnce(*mut xlib::Display, xlib::Window) -> T) -> Option<T> {
        std::env::var_os("DISPLAY")?;
        // SAFETY : connexion ouverte puis fermée ici ; NULL sans serveur X.
        unsafe {
            let display = xlib::XOpenDisplay(null());
            if display.is_null() {
                return None;
            }
            let result = f(display, xlib::XDefaultRootWindow(display));
            xlib::XCloseDisplay(display);
            Some(result)
        }
    }

    pub fn monitors() -> Vec<Monitor> {
        if wayland() {
            return Vec::new();
        }
        with_display(|display, root| {
            let mut count = 0;
            // SAFETY : tableau de `count` écrans renvoyé par XRandR, libéré après lecture.
            unsafe {
                let list = x11::xrandr::XRRGetMonitors(display, root, xlib::True, &mut count);
                if list.is_null() {
                    return Vec::new();
                }
                let monitors = std::slice::from_raw_parts(list, count.max(0) as usize)
                    .iter()
                    .map(|m| Monitor {
                        x: m.x,
                        y: m.y,
                        width: m.width,
                        height: m.height,
                    })
                    .collect();
                x11::xrandr::XRRFreeMonitors(list);
                monitors
            }
        })
        .unwrap_or_default()
    }

    pub fn cursor_position() -> Option<(i32, i32)> {
        if wayland() {
            return None;
        }
        pointer().map(|(x, y, _)| (x, y))
    }

    pub fn primary_button_down() -> bool {
        !wayland() && pointer().is_some_and(|(_, _, mask)| mask & xlib::Button1Mask != 0)
    }

    /// Position du curseur et état des boutons.
    fn pointer() -> Option<(i32, i32, c_uint)> {
        with_display(|display, root| {
            let (mut root_return, mut child) = (0, 0);
            let (mut x, mut y, mut wx, mut wy) = (0, 0, 0, 0);
            let mut mask: c_uint = 0;
            // SAFETY : connexion valide, pointeurs vers des variables locales.
            let ok = unsafe {
                xlib::XQueryPointer(
                    display,
                    root,
                    &mut root_return,
                    &mut child,
                    &mut x,
                    &mut y,
                    &mut wx,
                    &mut wy,
                    &mut mask,
                )
            };
            (ok != 0).then_some((x, y, mask))
        })
        .flatten()
    }

    pub fn warp(point: (i32, i32)) -> bool {
        if wayland() {
            return false;
        }
        with_display(|display, root| {
            // SAFETY : connexion valide.
            unsafe {
                xlib::XWarpPointer(display, 0, root, 0, 0, 0, 0, point.0, point.1);
                xlib::XFlush(display);
            }
        })
        .is_some()
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod imp {
    use super::Monitor;

    pub fn monitors() -> Vec<Monitor> {
        Vec::new()
    }

    pub fn cursor_position() -> Option<(i32, i32)> {
        None
    }

    pub fn warp(_point: (i32, i32)) -> bool {
        false
    }

    pub fn primary_button_down() -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(x: i32, y: i32, width: i32, height: i32) -> Monitor {
        Monitor {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn bounds_cover_all_monitors() {
        let monitors = [m(0, 0, 1920, 1080), m(-1280, 200, 1280, 1024)];
        assert_eq!(bounds(&monitors), Some(m(-1280, 0, 3200, 1224)));
        assert_eq!(bounds(&[]), None);
    }

    #[test]
    fn same_height_on_the_other_pc() {
        // Ce PC : un écran 1080p ; l'autre arrive par la gauche à 500 px du haut.
        let monitors = [m(0, 0, 1920, 1080)];
        assert_eq!(
            entry_point(&monitors, Position::Left, 500, 0),
            Some((2, 500))
        );
        // Bureau de l'autre PC décalé de 100 px vers le bas.
        assert_eq!(
            entry_point(&monitors, Position::Left, 500, 100),
            Some((2, 600))
        );
        // Hors de l'écran : ramené au bord le plus proche.
        assert_eq!(
            entry_point(&monitors, Position::Left, 2000, 0),
            Some((2, 1079))
        );
        assert_eq!(
            entry_point(&monitors, Position::Left, 10, -50),
            Some((2, 0))
        );
        assert_eq!(
            entry_point(&monitors, Position::Right, 0, 0),
            Some((1917, 0))
        );
        assert_eq!(
            entry_point(&monitors, Position::Top, 700, 0),
            Some((700, 2))
        );
        assert_eq!(
            entry_point(&monitors, Position::Bottom, 700, 0),
            Some((700, 1077))
        );
    }

    #[test]
    fn enters_on_the_monitor_of_the_edge() {
        // Deux écrans côte à côte, le gauche plus bas : l'autre PC est à droite.
        let monitors = [m(0, 300, 1280, 1024), m(1280, 0, 1920, 1080)];
        assert_eq!(
            entry_point(&monitors, Position::Right, 400, 0),
            Some((3197, 400))
        );
        // Plus bas que l'écran de droite : ramené en bas de celui-ci (le bord
        // droit du bureau n'existe que sur cet écran).
        assert_eq!(
            entry_point(&monitors, Position::Right, 1200, 0),
            Some((3197, 1079))
        );
        // L'autre PC à gauche : écran de gauche, même plus haut que lui.
        assert_eq!(
            entry_point(&monitors, Position::Left, 600, 0),
            Some((2, 600))
        );
        assert_eq!(
            entry_point(&monitors, Position::Left, 100, 0),
            Some((2, 300))
        );
    }

    #[test]
    fn along_is_relative_to_the_desktop() {
        let monitors = [m(-1280, -200, 1280, 1024), m(0, 0, 1920, 1080)];
        assert_eq!(along(&monitors, Position::Right, (1919, 300)), Some(500));
        assert_eq!(along(&monitors, Position::Top, (100, -200)), Some(1380));
    }
}

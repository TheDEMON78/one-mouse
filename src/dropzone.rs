//! Zone de dépôt au bord de l'écran, pour glisser des fichiers vers l'autre PC.
//!
//! Quand on glisse des fichiers jusqu'au bord où se trouve l'autre PC, une
//! petite fenêtre « Déposer ici » apparaît contre ce bord, sous le curseur. Les
//! fichiers lâchés dessus partent vers l'autre PC, où ils arrivent dans le
//! presse-papier et s'affichent dans l'explorateur de fichiers.
//!
//! Windows : fenêtre native du moteur (`WM_DROPFILES`), qui fonctionne même
//! quand la fenêtre one-mouse est fermée dans la zone de notification.
//! Ailleurs : la fenêtre one-mouse affiche la zone (voir `gui`).

use std::path::PathBuf;

/// Appelé avec les fichiers lâchés sur la zone.
pub type OnDrop = Box<dyn Fn(Vec<PathBuf>) + Send>;

/// Taille de la zone (pixels à 100 %).
pub const WIDTH: i32 = 300;
pub const HEIGHT: i32 = 130;

/// Rectangle de la zone contre le bord `side` (0 gauche, 1 droite, 2 haut,
/// 3 bas), contenant `point`, dans les limites de l'écran `screen`
/// (gauche, haut, droite, bas).
pub fn zone_rect(
    point: (i32, i32),
    side: u8,
    size: (i32, i32),
    screen: (i32, i32, i32, i32),
) -> (i32, i32, i32, i32) {
    let (w, h) = size;
    let (x, y) = match side {
        0 => (point.0, point.1 - h / 2),
        1 => (point.0 - w + 1, point.1 - h / 2),
        2 => (point.0 - w / 2, point.1),
        _ => (point.0 - w / 2, point.1 - h + 1),
    };
    let (left, top, right, bottom) = screen;
    let x = x.clamp(left, (right - w).max(left));
    let y = y.clamp(top, (bottom - h).max(top));
    (x, y, w, h)
}

pub struct DropZone {
    inner: imp::DropZone,
}

impl DropZone {
    /// Prépare la zone (cachée). `None` si ce système n'a pas de zone native.
    pub fn start(on_drop: OnDrop) -> Option<Self> {
        Some(Self {
            inner: imp::DropZone::start(on_drop)?,
        })
    }

    /// Montre la zone contre le bord `side`, sous le curseur en `point`. Elle se
    /// cache seule quand le bouton de la souris est relâché ailleurs.
    pub fn show(&self, point: (i32, i32), side: u8, peer: &str) {
        self.inner.show(point, side, peer);
    }
}

#[cfg(windows)]
mod imp {
    use std::cell::RefCell;
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::PathBuf;
    use std::ptr::{null, null_mut};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
    use windows_sys::Win32::Graphics::Gdi::{
        BeginPaint, CreateFontW, CreateSolidBrush, DT_CENTER, DT_WORDBREAK, DeleteObject,
        DrawTextW, EndPaint, FillRect, FrameRect, GetMonitorInfoW, InvalidateRect,
        MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint, PAINTSTRUCT, SelectObject,
        SetBkMode, SetTextColor, TRANSPARENT,
    };
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, VK_LBUTTON, VK_RBUTTON,
    };
    use windows_sys::Win32::UI::Shell::{DragFinish, DragQueryFileW, HDROP};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClientRect, GetMessageW,
        GetSystemMetrics, HWND_TOPMOST, IDC_ARROW, IsWindowVisible, LoadCursorW, MA_NOACTIVATE,
        MSG, PostMessageW, RegisterClassW, SM_SWAPBUTTON, SW_HIDE, SWP_NOACTIVATE, SWP_SHOWWINDOW,
        SetTimer, SetWindowPos, ShowWindow, TranslateMessage, WM_APP, WM_DROPFILES,
        WM_MOUSEACTIVATE, WM_PAINT, WM_TIMER, WNDCLASSW, WS_EX_ACCEPTFILES, WS_EX_NOACTIVATE,
        WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
    };

    use super::{HEIGHT, OnDrop, WIDTH, zone_rect};

    /// Message qui signale une commande en attente.
    const WM_ZONE_COMMAND: u32 = WM_APP + 1;
    /// La zone se cache quand le bouton est relâché depuis ce délai (le dépôt,
    /// lui, arrive aussitôt).
    const RELEASE_GRACE: Duration = Duration::from_millis(700);

    enum Command {
        Show {
            point: (i32, i32),
            side: u8,
            peer: String,
        },
    }

    struct Zone {
        commands: mpsc::Receiver<Command>,
        on_drop: OnDrop,
        text: Vec<u16>,
        scale: f32,
        released_at: Option<Instant>,
    }

    thread_local! {
        static ZONE: RefCell<Option<Zone>> = const { RefCell::new(None) };
    }

    pub struct DropZone {
        hwnd: usize,
        commands: mpsc::Sender<Command>,
    }

    fn wide(text: &str) -> Vec<u16> {
        std::ffi::OsStr::new(text)
            .encode_wide()
            .chain(Some(0))
            .collect()
    }

    /// Bouton principal de la souris enfoncé (boutons inversés pris en compte).
    fn primary_down() -> bool {
        // SAFETY : fonctions sans pointeur.
        unsafe {
            let key = if GetSystemMetrics(SM_SWAPBUTTON) != 0 {
                VK_RBUTTON
            } else {
                VK_LBUTTON
            };
            (GetAsyncKeyState(key as i32) as u16 & 0x8000) != 0
        }
    }

    impl DropZone {
        pub fn start(on_drop: OnDrop) -> Option<Self> {
            let (commands, commands_rx) = mpsc::channel();
            let (ready_tx, ready_rx) = mpsc::channel::<usize>();
            std::thread::Builder::new()
                .name("zone-depot".into())
                .spawn(move || run(commands_rx, on_drop, ready_tx))
                .ok()?;
            let hwnd = ready_rx.recv().ok().filter(|&h| h != 0)?;
            Some(Self { hwnd, commands })
        }

        pub fn show(&self, point: (i32, i32), side: u8, peer: &str) {
            let command = Command::Show {
                point,
                side,
                peer: peer.to_owned(),
            };
            if self.commands.send(command).is_ok() {
                // SAFETY : fenêtre du thread de la zone, valide tant qu'il tourne.
                unsafe {
                    PostMessageW(self.hwnd as HWND, WM_ZONE_COMMAND, 0, 0);
                }
            }
        }
    }

    fn run(commands: mpsc::Receiver<Command>, on_drop: OnDrop, ready: mpsc::Sender<usize>) {
        let class = wide("one-mouse-zone-depot");
        let title = wide("one-mouse");
        // SAFETY : classe et fenêtre créées et utilisées par ce seul thread ;
        // les chaînes vivent pendant les appels.
        let hwnd = unsafe {
            let instance = GetModuleHandleW(null());
            let wc = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: instance,
                lpszClassName: class.as_ptr(),
                hCursor: LoadCursorW(null_mut(), IDC_ARROW),
                ..std::mem::zeroed()
            };
            RegisterClassW(&wc);
            CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_ACCEPTFILES | WS_EX_NOACTIVATE,
                class.as_ptr(),
                title.as_ptr(),
                WS_POPUP,
                0,
                0,
                WIDTH,
                HEIGHT,
                null_mut(),
                null_mut(),
                instance,
                null(),
            )
        };
        if hwnd.is_null() {
            log::warn!("zone de dépôt indisponible");
            let _ = ready.send(0);
            return;
        }
        ZONE.with_borrow_mut(|zone| {
            *zone = Some(Zone {
                commands,
                on_drop,
                text: wide("Déposer ici"),
                scale: 1.0,
                released_at: None,
            });
        });
        // SAFETY : fenêtre créée ci-dessus, messages lus par ce thread.
        unsafe {
            SetTimer(hwnd, 1, 150, None);
            let _ = ready.send(hwnd as usize);
            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    /// Place et montre la zone contre le bord, à l'échelle de l'écran.
    unsafe fn show(hwnd: HWND, point: (i32, i32), side: u8, peer: &str) {
        // SAFETY : appels sur la fenêtre de ce thread et des variables locales.
        unsafe {
            let monitor = MonitorFromPoint(
                POINT {
                    x: point.0,
                    y: point.1,
                },
                MONITOR_DEFAULTTONEAREST,
            );
            let (mut dpi_x, mut dpi_y) = (96u32, 96u32);
            GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
            let scale = dpi_x.max(96) as f32 / 96.0;
            let mut info: MONITORINFO = std::mem::zeroed();
            info.cbSize = size_of::<MONITORINFO>() as u32;
            let screen = if GetMonitorInfoW(monitor, &mut info) != 0 {
                let r = info.rcMonitor;
                (r.left, r.top, r.right, r.bottom)
            } else {
                (i32::MIN / 2, i32::MIN / 2, i32::MAX / 2, i32::MAX / 2)
            };
            let size = (
                (WIDTH as f32 * scale) as i32,
                (HEIGHT as f32 * scale) as i32,
            );
            let (x, y, w, h) = zone_rect(point, side, size, screen);
            ZONE.with_borrow_mut(|zone| {
                if let Some(zone) = zone {
                    zone.text = wide(&format!("Déposer ici\npour envoyer à « {peer} »"));
                    zone.scale = scale;
                    zone.released_at = None;
                }
            });
            SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                x,
                y,
                w,
                h,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
            InvalidateRect(hwnd, null(), 1);
        }
    }

    /// Fichiers d'un dépôt.
    unsafe fn dropped_files(drop: HDROP) -> Vec<PathBuf> {
        let mut files = Vec::new();
        // SAFETY : `drop` vient de WM_DROPFILES ; tampons de la taille annoncée.
        unsafe {
            let count = DragQueryFileW(drop, u32::MAX, null_mut(), 0);
            for i in 0..count {
                let len = DragQueryFileW(drop, i, null_mut(), 0);
                let mut buffer = vec![0u16; len as usize + 1];
                let copied = DragQueryFileW(drop, i, buffer.as_mut_ptr(), len + 1);
                buffer.truncate(copied as usize);
                files.push(PathBuf::from(OsString::from_wide(&buffer)));
            }
            DragFinish(drop);
        }
        files
    }

    unsafe fn paint(hwnd: HWND) {
        let (text, scale) = ZONE.with_borrow(|zone| {
            zone.as_ref()
                .map(|z| (z.text.clone(), z.scale))
                .unwrap_or_else(|| (wide("Déposer ici"), 1.0))
        });
        // SAFETY : peinture de la fenêtre de ce thread ; objets GDI créés ici
        // puis supprimés.
        unsafe {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let hdc = BeginPaint(hwnd, &mut ps);
            let mut rect: RECT = std::mem::zeroed();
            GetClientRect(hwnd, &mut rect);
            // Couleurs de la fenêtre one-mouse (format 0x00BBGGRR).
            let background = CreateSolidBrush(0x0029_1E1A);
            let accent = CreateSolidBrush(0x00FF_8C58);
            FillRect(hdc, &rect, background);
            for inset in 0..(3.0 * scale) as i32 {
                let frame = RECT {
                    left: rect.left + inset,
                    top: rect.top + inset,
                    right: rect.right - inset,
                    bottom: rect.bottom - inset,
                };
                FrameRect(hdc, &frame, accent);
            }
            let face = wide("Segoe UI");
            let font = CreateFontW(
                -(20.0 * scale) as i32,
                0,
                0,
                0,
                600,
                0,
                0,
                0,
                1,
                0,
                0,
                5,
                0,
                face.as_ptr(),
            );
            let old_font = SelectObject(hdc, font);
            SetBkMode(hdc, TRANSPARENT as i32);
            SetTextColor(hdc, 0x00F2_EAE6);
            // Texte centré verticalement : hauteur mesurée d'abord.
            let mut measure = rect;
            measure.left += (12.0 * scale) as i32;
            measure.right -= (12.0 * scale) as i32;
            let height = DrawTextW(
                hdc,
                text.as_ptr(),
                -1,
                &mut measure,
                DT_CENTER | DT_WORDBREAK | windows_sys::Win32::Graphics::Gdi::DT_CALCRECT,
            );
            let mut target = rect;
            target.left += (12.0 * scale) as i32;
            target.right -= (12.0 * scale) as i32;
            target.top += ((rect.bottom - rect.top) - height).max(0) / 2;
            DrawTextW(
                hdc,
                text.as_ptr(),
                -1,
                &mut target,
                DT_CENTER | DT_WORDBREAK,
            );
            SelectObject(hdc, old_font);
            DeleteObject(font);
            DeleteObject(background);
            DeleteObject(accent);
            EndPaint(hwnd, &ps);
        }
    }

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        // SAFETY : messages de la fenêtre de ce thread.
        unsafe {
            match msg {
                WM_ZONE_COMMAND => {
                    let commands: Vec<Command> = ZONE.with_borrow(|zone| {
                        zone.as_ref()
                            .map(|z| z.commands.try_iter().collect())
                            .unwrap_or_default()
                    });
                    // Seule la dernière position compte.
                    if let Some(Command::Show { point, side, peer }) = commands.into_iter().last() {
                        show(hwnd, point, side, &peer);
                    }
                    0
                }
                WM_DROPFILES => {
                    let files = dropped_files(wparam as HDROP);
                    ShowWindow(hwnd, SW_HIDE);
                    if !files.is_empty() {
                        ZONE.with_borrow(|zone| {
                            if let Some(zone) = zone {
                                (zone.on_drop)(files);
                            }
                        });
                    }
                    0
                }
                WM_TIMER => {
                    if IsWindowVisible(hwnd) != 0 {
                        let hide = ZONE.with_borrow_mut(|zone| {
                            let Some(zone) = zone else {
                                return true;
                            };
                            if primary_down() {
                                zone.released_at = None;
                                false
                            } else {
                                zone.released_at.get_or_insert_with(Instant::now).elapsed()
                                    >= RELEASE_GRACE
                            }
                        });
                        if hide {
                            ShowWindow(hwnd, SW_HIDE);
                        }
                    }
                    0
                }
                WM_PAINT => {
                    paint(hwnd);
                    0
                }
                WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
                _ => DefWindowProcW(hwnd, msg, wparam, lparam),
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::OnDrop;

    pub struct DropZone;

    impl DropZone {
        pub fn start(_on_drop: OnDrop) -> Option<Self> {
            None
        }

        pub fn show(&self, _point: (i32, i32), _side: u8, _peer: &str) {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zone_sits_against_the_edge_under_the_cursor() {
        let screen = (0, 0, 1920, 1080);
        // Bord droit : la zone finit au bord, centrée sur le curseur.
        assert_eq!(
            zone_rect((1919, 500), 1, (300, 130), screen),
            (1620, 435, 300, 130)
        );
        // Bord gauche, tout en haut : ramenée dans l'écran.
        assert_eq!(zone_rect((0, 10), 0, (300, 130), screen), (0, 0, 300, 130));
        // Bord bas.
        assert_eq!(
            zone_rect((960, 1079), 3, (300, 130), screen),
            (810, 950, 300, 130)
        );
    }
}

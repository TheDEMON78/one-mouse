//! Capture de la souris et du clavier sous Linux X11, écrite pour one-mouse
//! (la bibliothèque `input-capture` ne la fournit pas).
//!
//! Principe (comme Synergy / Barrier) :
//! - XInput2 signale chaque mouvement brut de la souris ; la position du curseur
//!   indique alors s'il pousse contre un bord où se trouve l'autre PC.
//! - Au bord : la souris et le clavier sont réservés à one-mouse (`XGrabPointer`,
//!   `XGrabKeyboard`) et leurs événements lui sont livrés ; le curseur est masqué
//!   et ramené au centre après chaque mouvement, l'écart au centre donnant le
//!   déplacement à envoyer.
//! - À la libération, le curseur réapparaît là où il avait quitté l'écran.
//!
//! Xlib n'étant pas thread-safe, tout se passe dans un thread dédié qui possède
//! sa propre connexion au serveur X.

use std::collections::HashSet;
use std::os::raw::{c_char, c_int, c_uint, c_ulong};
use std::ptr;
use std::sync::mpsc as std_mpsc;

use anyhow::{Result, anyhow, bail};
use input_capture::{CaptureEvent, Position};
use input_event::{
    BTN_BACK, BTN_FORWARD, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, Event, KeyboardEvent, PointerEvent,
};
use tokio::sync::mpsc;
use x11::{xinput2, xlib};

/// Délai maximal de prise en compte d'une commande (libération, arrêt).
const POLL_MS: c_int = 10;
/// Une coche de molette dans la convention `AxisDiscrete120`.
const WHEEL_STEP: i32 = 120;

pub enum Command {
    Create(Position),
    Destroy(Position),
    /// Rend la main ; le curseur réapparaît au point donné, sinon là où il était sorti.
    Release(Option<(i32, i32)>),
    Terminate,
}

/// Événement envoyé au reste du programme, avec, pour `Begin`, le point où le
/// curseur a quitté l'écran.
pub type Output = (Position, CaptureEvent, Option<(i32, i32)>);

/// Démarre le thread de capture. Échoue si le serveur X ou XInput2 sont indisponibles.
pub fn spawn() -> Result<(std_mpsc::Sender<Command>, mpsc::UnboundedReceiver<Output>)> {
    let (cmd_tx, cmd_rx) = std_mpsc::channel();
    let (out_tx, out_rx) = mpsc::unbounded_channel();
    let (ready_tx, ready_rx) = std_mpsc::channel::<Result<()>>();
    std::thread::Builder::new()
        .name("capture-x11".into())
        .spawn(move || match X11::open() {
            Ok(mut x) => {
                let _ = ready_tx.send(Ok(()));
                x.run(cmd_rx, out_tx);
            }
            Err(e) => {
                let _ = ready_tx.send(Err(e));
            }
        })?;
    ready_rx
        .recv()
        .map_err(|_| anyhow!("le thread X11 s'est arrêté"))??;
    Ok((cmd_tx, out_rx))
}

struct X11 {
    display: *mut xlib::Display,
    root: c_ulong,
    xi_opcode: c_int,
    invisible_cursor: c_ulong,
    width: i32,
    height: i32,
    /// Bords où se trouve un autre PC.
    positions: HashSet<Position>,
    /// Capture active : bord franchi et point de sortie du curseur.
    grabbed: Option<(Position, i32, i32)>,
    last: (i32, i32),
}

/// Les erreurs X (ex. fenêtre disparue) ne doivent pas arrêter le programme.
unsafe extern "C" fn on_x_error(_: *mut xlib::Display, event: *mut xlib::XErrorEvent) -> c_int {
    // SAFETY : Xlib fournit un pointeur valide pendant l'appel.
    let code = unsafe { (*event).error_code };
    log::debug!("erreur X ignorée (code {code})");
    0
}

impl X11 {
    fn open() -> Result<Self> {
        // SAFETY : appels Xlib sur une connexion ouverte ici et utilisée par ce seul thread.
        unsafe {
            xlib::XSetErrorHandler(Some(on_x_error));
            let display = xlib::XOpenDisplay(ptr::null());
            if display.is_null() {
                bail!("impossible d'ouvrir l'affichage X11 (variable DISPLAY ?)");
            }
            let root = xlib::XDefaultRootWindow(display);

            let (mut opcode, mut first_event, mut first_error) = (0, 0, 0);
            if xlib::XQueryExtension(
                display,
                c"XInputExtension".as_ptr(),
                &mut opcode,
                &mut first_event,
                &mut first_error,
            ) == 0
            {
                xlib::XCloseDisplay(display);
                bail!("extension XInput absente");
            }
            let (mut major, mut minor) = (2, 0);
            if xinput2::XIQueryVersion(display, &mut major, &mut minor) != xlib::Success as c_int {
                xlib::XCloseDisplay(display);
                bail!("XInput2 indisponible");
            }

            // Mouvements bruts de toutes les souris, même quand le curseur est bloqué au bord.
            let mut mask = [0u8; ((xinput2::XI_LASTEVENT + 8) / 8) as usize];
            let event = xinput2::XI_RawMotion;
            mask[(event >> 3) as usize] |= 1 << (event & 7);
            let mut event_mask = xinput2::XIEventMask {
                deviceid: xinput2::XIAllMasterDevices,
                mask_len: mask.len() as c_int,
                mask: mask.as_mut_ptr(),
            };
            xinput2::XISelectEvents(display, root, &mut event_mask, 1);
            // Changements de taille de l'écran (ajout d'un moniteur, rotation…).
            xlib::XSelectInput(display, root, xlib::StructureNotifyMask);

            // Curseur invisible affiché pendant la capture.
            let data: [c_char; 1] = [0];
            let pixmap = xlib::XCreateBitmapFromData(display, root, data.as_ptr(), 1, 1);
            let mut color: xlib::XColor = std::mem::zeroed();
            let invisible_cursor =
                xlib::XCreatePixmapCursor(display, pixmap, pixmap, &mut color, &mut color, 0, 0);
            xlib::XFreePixmap(display, pixmap);

            let mut x = Self {
                display,
                root,
                xi_opcode: opcode,
                invisible_cursor,
                width: 0,
                height: 0,
                positions: HashSet::new(),
                grabbed: None,
                last: (0, 0),
            };
            x.update_size();
            x.last = x.pointer();
            xlib::XFlush(display);
            log::info!("capture X11 prête ({}x{})", x.width, x.height);
            Ok(x)
        }
    }

    fn update_size(&mut self) {
        // SAFETY : connexion valide, pointeurs vers des variables locales.
        unsafe {
            let (mut root, mut x, mut y, mut w, mut h, mut border, mut depth) =
                (0, 0, 0, 0, 0, 0, 0);
            xlib::XGetGeometry(
                self.display,
                self.root,
                &mut root,
                &mut x,
                &mut y,
                &mut w,
                &mut h,
                &mut border,
                &mut depth,
            );
            self.width = w as i32;
            self.height = h as i32;
        }
    }

    fn pointer(&self) -> (i32, i32) {
        // SAFETY : connexion valide, pointeurs vers des variables locales.
        unsafe {
            let (mut root, mut child) = (0, 0);
            let (mut x, mut y, mut wx, mut wy) = (0, 0, 0, 0);
            let mut mask: c_uint = 0;
            xlib::XQueryPointer(
                self.display,
                self.root,
                &mut root,
                &mut child,
                &mut x,
                &mut y,
                &mut wx,
                &mut wy,
                &mut mask,
            );
            (x, y)
        }
    }

    fn warp(&self, x: i32, y: i32) {
        // SAFETY : connexion valide.
        unsafe {
            xlib::XWarpPointer(self.display, 0, self.root, 0, 0, 0, 0, x, y);
        }
    }

    fn center(&self) -> (i32, i32) {
        (self.width / 2, self.height / 2)
    }

    fn run(&mut self, commands: std_mpsc::Receiver<Command>, out: mpsc::UnboundedSender<Output>) {
        // SAFETY : connexion valide pour toute la durée de la boucle.
        let fd = unsafe { xlib::XConnectionNumber(self.display) };
        loop {
            loop {
                match commands.try_recv() {
                    Ok(Command::Create(pos)) => {
                        self.positions.insert(pos);
                    }
                    Ok(Command::Destroy(pos)) => {
                        self.positions.remove(&pos);
                        if self.grabbed.is_some_and(|(p, _, _)| p == pos) {
                            self.release(None);
                        }
                    }
                    Ok(Command::Release(point)) => self.release(point),
                    Ok(Command::Terminate) | Err(std_mpsc::TryRecvError::Disconnected) => {
                        self.release(None);
                        // SAFETY : plus aucun appel Xlib après la fermeture.
                        unsafe {
                            xlib::XFreeCursor(self.display, self.invisible_cursor);
                            xlib::XCloseDisplay(self.display);
                        }
                        return;
                    }
                    Err(std_mpsc::TryRecvError::Empty) => break,
                }
            }

            let mut pollfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY : un seul descripteur valide.
            unsafe { libc::poll(&mut pollfd, 1, POLL_MS) };

            let mut moved = false;
            let mut push = (0.0f64, 0.0f64);
            // SAFETY : lecture des événements de notre connexion ; les données
            // des événements génériques sont libérées après usage.
            unsafe {
                while xlib::XPending(self.display) > 0 {
                    let mut event: xlib::XEvent = std::mem::zeroed();
                    xlib::XNextEvent(self.display, &mut event);
                    match event.get_type() {
                        xlib::ConfigureNotify => self.update_size(),
                        // Pendant la capture, la souris et le clavier réservés envoient
                        // leurs événements classiques à one-mouse.
                        xlib::MotionNotify => moved = true,
                        xlib::ButtonPress | xlib::ButtonRelease => {
                            let pressed = event.get_type() == xlib::ButtonPress;
                            if let Some(event) = button_event(event.button.button as c_int, pressed)
                            {
                                self.emit(&out, event);
                            }
                        }
                        xlib::KeyPress | xlib::KeyRelease => {
                            let pressed = event.get_type() == xlib::KeyPress;
                            // Code X = code evdev + 8.
                            let key = event.key.keycode.saturating_sub(8);
                            self.emit(
                                &out,
                                Event::Keyboard(KeyboardEvent::Key {
                                    time: 0,
                                    key,
                                    state: pressed as u8,
                                }),
                            );
                        }
                        // Hors capture : mouvements bruts, pour repérer la poussée contre un bord.
                        xlib::GenericEvent => {
                            let mut cookie: xlib::XGenericEventCookie = event.generic_event_cookie;
                            if cookie.extension != self.xi_opcode
                                || xlib::XGetEventData(self.display, &mut cookie) == 0
                            {
                                continue;
                            }
                            if cookie.evtype == xinput2::XI_RawMotion {
                                moved = true;
                                let raw = &*(cookie.data as *const xinput2::XIRawEvent);
                                let (dx, dy) = raw_motion(raw);
                                push.0 += dx;
                                push.1 += dy;
                            }
                            xlib::XFreeEventData(self.display, &mut cookie);
                        }
                        _ => {}
                    }
                }
            }

            if moved {
                self.on_motion(push, &out);
            }
            // SAFETY : connexion valide.
            unsafe { xlib::XFlush(self.display) };
        }
    }

    fn emit(&self, out: &mpsc::UnboundedSender<Output>, event: Event) {
        if let Some((pos, _, _)) = self.grabbed {
            let _ = out.send((pos, CaptureEvent::Input(event), None));
        }
    }

    fn on_motion(&mut self, push: (f64, f64), out: &mpsc::UnboundedSender<Output>) {
        let (x, y) = self.pointer();
        if let Some((pos, _, _)) = self.grabbed {
            let (cx, cy) = self.center();
            let (dx, dy) = (x - cx, y - cy);
            if dx != 0 || dy != 0 {
                let _ = out.send((
                    pos,
                    CaptureEvent::Input(Event::Pointer(PointerEvent::Motion {
                        time: 0,
                        dx: dx as f64,
                        dy: dy as f64,
                    })),
                    None,
                ));
                self.warp(cx, cy);
            }
            return;
        }

        // Direction du mouvement : position du curseur, ou mouvement brut quand
        // le curseur est déjà bloqué contre le bord.
        let dx = if x != self.last.0 {
            (x - self.last.0) as f64
        } else {
            push.0
        };
        let dy = if y != self.last.1 {
            (y - self.last.1) as f64
        } else {
            push.1
        };
        self.last = (x, y);
        let hit = self.positions.iter().copied().find(|pos| match pos {
            Position::Left => x <= 0 && dx < 0.0,
            Position::Right => x >= self.width - 1 && dx > 0.0,
            Position::Top => y <= 0 && dy < 0.0,
            Position::Bottom => y >= self.height - 1 && dy > 0.0,
        });
        if let Some(pos) = hit
            && self.grab()
        {
            self.grabbed = Some((pos, x, y));
            let (cx, cy) = self.center();
            self.warp(cx, cy);
            let _ = out.send((pos, CaptureEvent::Begin, Some((x, y))));
        }
    }

    /// Réserve souris et clavier ; échoue si une autre application les tient déjà
    /// (menu ouvert, glisser-déposer en cours…).
    fn grab(&self) -> bool {
        // SAFETY : connexion et fenêtre racine valides.
        unsafe {
            let pointer = xlib::XGrabPointer(
                self.display,
                self.root,
                xlib::False,
                (xlib::ButtonPressMask | xlib::ButtonReleaseMask | xlib::PointerMotionMask)
                    as c_uint,
                xlib::GrabModeAsync,
                xlib::GrabModeAsync,
                0,
                self.invisible_cursor,
                xlib::CurrentTime,
            );
            if pointer != xlib::GrabSuccess {
                log::debug!("souris déjà réservée par une autre application");
                return false;
            }
            let keyboard = xlib::XGrabKeyboard(
                self.display,
                self.root,
                xlib::False,
                xlib::GrabModeAsync,
                xlib::GrabModeAsync,
                xlib::CurrentTime,
            );
            if keyboard != xlib::GrabSuccess {
                xlib::XUngrabPointer(self.display, xlib::CurrentTime);
                log::debug!("clavier déjà réservé par une autre application");
                return false;
            }
            true
        }
    }

    fn release(&mut self, point: Option<(i32, i32)>) {
        let Some((pos, x, y)) = self.grabbed.take() else {
            return;
        };
        // SAFETY : connexion valide.
        unsafe {
            xlib::XUngrabKeyboard(self.display, xlib::CurrentTime);
            xlib::XUngrabPointer(self.display, xlib::CurrentTime);
        }
        // Le curseur revient au point demandé, sinon là où il est sorti,
        // légèrement à l'intérieur de l'écran.
        let (x, y) = point.unwrap_or(match pos {
            Position::Left => (x.max(0) + 2, y),
            Position::Right => (x.min(self.width - 1) - 2, y),
            Position::Top => (x, y.max(0) + 2),
            Position::Bottom => (x, y.min(self.height - 1) - 2),
        });
        self.warp(x, y);
        self.last = (x, y);
        // SAFETY : connexion valide.
        unsafe { xlib::XFlush(self.display) };
    }
}

/// Mouvement brut (non accéléré) sur les axes X et Y d'un événement XInput2.
fn raw_motion(raw: &xinput2::XIRawEvent) -> (f64, f64) {
    let mut values = [0.0f64; 2];
    let mut index = 0;
    for axis in 0..(raw.valuators.mask_len * 8) {
        // SAFETY : `mask` contient `mask_len` octets et `raw_values` une valeur
        // par bit actif, dans l'ordre.
        let set = unsafe { *raw.valuators.mask.add((axis / 8) as usize) } & (1 << (axis % 8)) != 0;
        if !set {
            continue;
        }
        if axis < 2 {
            values[axis as usize] = unsafe { *raw.raw_values.add(index) };
        }
        index += 1;
        if axis >= 1 {
            break;
        }
    }
    (values[0], values[1])
}

/// Boutons X11 : 1 gauche, 2 milieu, 3 droit, 4-7 molette, 8-9 précédent / suivant.
fn button_event(button: c_int, pressed: bool) -> Option<Event> {
    let pointer_button = |code| {
        Some(Event::Pointer(PointerEvent::Button {
            time: 0,
            button: code,
            state: pressed as u32,
        }))
    };
    let wheel = |axis, value| {
        // La molette n'émet qu'au clic : on ignore le relâchement.
        pressed.then_some(Event::Pointer(PointerEvent::AxisDiscrete120 {
            axis,
            value,
        }))
    };
    match button {
        1 => pointer_button(BTN_LEFT),
        2 => pointer_button(BTN_MIDDLE),
        3 => pointer_button(BTN_RIGHT),
        4 => wheel(0, -WHEEL_STEP),
        5 => wheel(0, WHEEL_STEP),
        6 => wheel(1, -WHEEL_STEP),
        7 => wheel(1, WHEEL_STEP),
        8 => pointer_button(BTN_BACK),
        9 => pointer_button(BTN_FORWARD),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buttons_are_mapped() {
        assert_eq!(
            button_event(1, true),
            Some(Event::Pointer(PointerEvent::Button {
                time: 0,
                button: BTN_LEFT,
                state: 1
            }))
        );
        assert_eq!(
            button_event(5, true),
            Some(Event::Pointer(PointerEvent::AxisDiscrete120 {
                axis: 0,
                value: 120
            }))
        );
        assert_eq!(button_event(5, false), None);
        assert_eq!(button_event(42, true), None);
    }
}

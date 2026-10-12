//! Capture de la souris et du clavier : bibliothèque `input-capture` (Windows,
//! macOS, Wayland) ou capture X11 propre à one-mouse.

#[cfg(target_os = "linux")]
mod x11;

use std::collections::HashSet;

use anyhow::Result;
use futures::StreamExt;
use input_capture::{CaptureEvent, InputCapture, Position};
use input_event::scancode;

pub type Handle = u64;

pub enum Capture {
    Lib(InputCapture),
    #[cfg(target_os = "linux")]
    X11(X11Capture),
}

impl Capture {
    /// Choisit la meilleure capture disponible ; `None` si aucune ne fonctionne
    /// (le PC peut alors seulement être contrôlé).
    pub async fn new() -> Option<Self> {
        let lib_error = match InputCapture::new(None).await {
            Ok(capture) => return Some(Capture::Lib(capture)),
            Err(e) => e,
        };
        #[cfg(target_os = "linux")]
        if std::env::var_os("DISPLAY").is_some() {
            match X11Capture::new() {
                Ok(capture) => return Some(Capture::X11(capture)),
                Err(e) => log::warn!("capture X11 indisponible : {e:#}"),
            }
        }
        log::warn!("capture indisponible ({lib_error}) : ce PC fonctionnera en réception seule");
        None
    }

    pub async fn create(&mut self, handle: Handle, pos: Position) -> Result<()> {
        match self {
            Capture::Lib(c) => Ok(c.create(handle, pos).await?),
            #[cfg(target_os = "linux")]
            Capture::X11(c) => c.create(handle, pos),
        }
    }

    pub async fn destroy(&mut self, handle: Handle) -> Result<()> {
        match self {
            Capture::Lib(c) => Ok(c.destroy(handle).await?),
            #[cfg(target_os = "linux")]
            Capture::X11(c) => c.destroy(handle),
        }
    }

    pub async fn release(&mut self) -> Result<()> {
        match self {
            Capture::Lib(c) => Ok(c.release().await?),
            #[cfg(target_os = "linux")]
            Capture::X11(c) => c.release(),
        }
    }

    pub async fn terminate(&mut self) -> Result<()> {
        match self {
            Capture::Lib(c) => Ok(c.terminate().await?),
            #[cfg(target_os = "linux")]
            Capture::X11(c) => c.terminate(),
        }
    }

    pub fn keys_pressed(&self, keys: &[scancode::Linux]) -> bool {
        match self {
            Capture::Lib(c) => c.keys_pressed(keys),
            #[cfg(target_os = "linux")]
            Capture::X11(c) => keys.iter().all(|k| c.pressed_keys.contains(k)),
        }
    }

    /// Touches encore enfoncées chez l'autre PC, à relâcher quand on reprend la main.
    pub fn take_pressed_keys(&mut self) -> HashSet<scancode::Linux> {
        match self {
            Capture::Lib(c) => c.take_pressed_keys(),
            #[cfg(target_os = "linux")]
            Capture::X11(c) => std::mem::take(&mut c.pressed_keys),
        }
    }

    pub async fn next(&mut self) -> Option<Result<(Handle, CaptureEvent)>> {
        match self {
            Capture::Lib(c) => c.next().await.map(|r| r.map_err(Into::into)),
            #[cfg(target_os = "linux")]
            Capture::X11(c) => c.next().await.map(Ok),
        }
    }
}

#[cfg(target_os = "linux")]
pub struct X11Capture {
    commands: std::sync::mpsc::Sender<x11::Command>,
    events: tokio::sync::mpsc::UnboundedReceiver<x11::Output>,
    handles: std::collections::HashMap<Handle, Position>,
    pressed_keys: HashSet<scancode::Linux>,
}

#[cfg(target_os = "linux")]
impl X11Capture {
    fn new() -> Result<Self> {
        let (commands, events) = x11::spawn()?;
        Ok(Self {
            commands,
            events,
            handles: std::collections::HashMap::new(),
            pressed_keys: HashSet::new(),
        })
    }

    fn send(&self, command: x11::Command) -> Result<()> {
        self.commands
            .send(command)
            .map_err(|_| anyhow::anyhow!("la capture X11 s'est arrêtée"))
    }

    fn create(&mut self, handle: Handle, pos: Position) -> Result<()> {
        self.handles.insert(handle, pos);
        self.send(x11::Command::Create(pos))
    }

    fn destroy(&mut self, handle: Handle) -> Result<()> {
        match self.handles.remove(&handle) {
            Some(pos) if !self.handles.values().any(|&p| p == pos) => {
                self.send(x11::Command::Destroy(pos))
            }
            _ => Ok(()),
        }
    }

    fn release(&mut self) -> Result<()> {
        self.pressed_keys.clear();
        self.send(x11::Command::Release)
    }

    fn terminate(&mut self) -> Result<()> {
        self.send(x11::Command::Terminate)
    }

    async fn next(&mut self) -> Option<(Handle, CaptureEvent)> {
        loop {
            let (pos, event) = self.events.recv().await?;
            use input_event::{Event, KeyboardEvent};
            if let CaptureEvent::Input(Event::Keyboard(KeyboardEvent::Key { key, state, .. })) =
                event
                && let Ok(key) = scancode::Linux::try_from(key)
            {
                if state == 0 {
                    self.pressed_keys.remove(&key);
                } else {
                    self.pressed_keys.insert(key);
                }
            }
            // Événements d'un bord supprimé entre-temps : ignorés.
            if let Some((&handle, _)) = self.handles.iter().find(|&(_, &p)| p == pos) {
                return Some((handle, event));
            }
        }
    }
}

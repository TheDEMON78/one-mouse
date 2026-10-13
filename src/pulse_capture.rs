//! Capture du son sous Linux par libpulse, la bibliothèque officielle de
//! PulseAudio (et de PipeWire en compatibilité), comme `parec`.
//!
//! Le connecteur PulseAudio de cpal, réécrit en Rust, s'interrompt parfois
//! plusieurs secondes ou livre moins de son que le temps réel : mesuré sur la
//! même source, libpulse livre exactement 100 paquets de 10 ms par seconde
//! quand cpal en livre de 0 à 196. cpal reste utilisé pour lister et choisir
//! les périphériques ; seul le flux de capture passe par libpulse.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;

use anyhow::{Result, anyhow};
use libpulse_binding::def::BufferAttr;
use libpulse_binding::sample::{Format, Spec};
use libpulse_binding::stream::Direction;
use libpulse_simple_binding::Simple;
use tokio::sync::mpsc;

use crate::audio::AudioFrame;

/// Flux de capture ouvert sur une source PulseAudio.
pub struct PulseCapture {
    simple: Simple,
    rate: u32,
    channels: u16,
    frame_ms: u32,
}

impl PulseCapture {
    /// Ouvre la source `name` (nom PulseAudio, ex. « …monitor »), en flottants
    /// 32 bits ; le serveur convertit si besoin. `frame_ms` : durée d'un paquet.
    pub fn open(name: &str, rate: u32, channels: u16, frame_ms: u32) -> Result<Self> {
        let spec = Spec {
            format: Format::F32le,
            channels: channels.clamp(1, 32) as u8,
            rate,
        };
        if !spec.is_valid() {
            return Err(anyhow!(
                "format audio refusé ({rate} Hz, {channels} canaux)"
            ));
        }
        let fragment = rate * frame_ms / 1000 * spec.frame_size() as u32;
        // Le serveur livre un paquet par fragment : peu de retard, et u32::MAX
        // (« valeur par défaut ») pour le reste.
        let attr = BufferAttr {
            maxlength: u32::MAX,
            tlength: u32::MAX,
            prebuf: u32::MAX,
            minreq: u32::MAX,
            fragsize: fragment,
        };
        let simple = Simple::new(
            None,
            "one-mouse",
            Direction::Record,
            Some(name),
            "son partagé avec l'autre PC",
            &spec,
            None,
            Some(&attr),
        )
        .map_err(|e| anyhow!("capture de « {name} » impossible : {e}"))?;
        Ok(Self {
            simple,
            rate,
            channels: spec.channels as u16,
            frame_ms,
        })
    }

    /// Lit le son jusqu'à l'arrêt (message ou destruction de l'émetteur de
    /// `stop`) ; `broken` signale une erreur de flux (source disparue…).
    pub fn run(
        self,
        tx: mpsc::Sender<AudioFrame>,
        stop: std_mpsc::Receiver<()>,
        broken: Arc<AtomicBool>,
    ) {
        let samples = (self.rate * self.frame_ms / 1000) as usize * self.channels as usize;
        let mut bytes = vec![0u8; samples * 4];
        loop {
            match stop.try_recv() {
                Err(std_mpsc::TryRecvError::Empty) => {}
                _ => return,
            }
            if let Err(e) = self.simple.read(&mut bytes) {
                log::warn!("capture audio : {e} (relance)");
                broken.store(true, Ordering::Relaxed);
                return;
            }
            let frame = AudioFrame {
                rate: self.rate,
                channels: self.channels,
                samples: bytes
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|&b| f32::from_le_bytes(b))
                    .collect(),
            };
            // Réseau saturé : on perd ce paquet plutôt que d'accumuler du retard.
            let _ = tx.try_send(frame);
        }
    }
}

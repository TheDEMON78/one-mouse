//! Messages échangés entre les deux PC.

use input_event::{Event, KeyboardEvent, PointerEvent};
use serde::{Deserialize, Serialize};

use crate::audio::AudioFrame;

/// Version du protocole, incrémentée à chaque changement incompatible.
pub const PROTOCOL_VERSION: u32 = 1;

/// Taille maximale d'un texte de presse-papier synchronisé (512 Kio).
pub const MAX_CLIPBOARD_LEN: usize = 512 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Message {
    /// Premier message chiffré : prouve que les deux côtés ont le même mot de passe.
    Hello { version: u32, name: String },
    /// L'émetteur commence à envoyer ses entrées : le récepteur prend la main.
    Enter,
    /// Un événement souris ou clavier à rejouer sur le récepteur.
    Input(WireEvent),
    /// L'émetteur arrête d'envoyer ses entrées (retour du curseur chez lui).
    Exit,
    /// Le récepteur demande à l'émetteur d'arrêter (le curseur a franchi le bord du retour).
    Leave,
    /// Nouveau texte copié dans le presse-papier.
    Clipboard(String),
    /// Paquet de son (PCM 16 bits little-endian entrelacé).
    Audio {
        kind: AudioKind,
        rate: u32,
        channels: u16,
        pcm: Vec<u8>,
    },
    /// Maintien de la connexion.
    Ping,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioKind {
    /// Son du système (ce qui sort des haut-parleurs).
    Sound,
    /// Micro.
    Mic,
}

impl Message {
    pub fn audio(kind: AudioKind, frame: &AudioFrame) -> Self {
        Message::Audio {
            kind,
            rate: frame.rate,
            channels: frame.channels,
            pcm: frame.samples.iter().flat_map(|s| s.to_le_bytes()).collect(),
        }
    }
}

/// Reconstitue un paquet de son reçu ; `None` s'il est invalide.
pub fn audio_frame(rate: u32, channels: u16, pcm: &[u8]) -> Option<AudioFrame> {
    if !pcm.len().is_multiple_of(2) {
        return None;
    }
    let frame = AudioFrame {
        rate,
        channels,
        samples: pcm
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&b| i16::from_le_bytes(b))
            .collect(),
    };
    frame.is_valid().then_some(frame)
}

/// Équivalent sérialisable de [`input_event::Event`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum WireEvent {
    Motion {
        dx: f64,
        dy: f64,
    },
    Button {
        button: u32,
        state: u32,
    },
    Axis {
        axis: u8,
        value: f64,
    },
    AxisDiscrete120 {
        axis: u8,
        value: i32,
    },
    Key {
        key: u32,
        state: u8,
    },
    Modifiers {
        depressed: u32,
        latched: u32,
        locked: u32,
        group: u32,
    },
}

impl From<Event> for WireEvent {
    fn from(event: Event) -> Self {
        match event {
            Event::Pointer(PointerEvent::Motion { dx, dy, .. }) => WireEvent::Motion { dx, dy },
            Event::Pointer(PointerEvent::Button { button, state, .. }) => {
                WireEvent::Button { button, state }
            }
            Event::Pointer(PointerEvent::Axis { axis, value, .. }) => {
                WireEvent::Axis { axis, value }
            }
            Event::Pointer(PointerEvent::AxisDiscrete120 { axis, value }) => {
                WireEvent::AxisDiscrete120 { axis, value }
            }
            Event::Keyboard(KeyboardEvent::Key { key, state, .. }) => WireEvent::Key { key, state },
            Event::Keyboard(KeyboardEvent::Modifiers {
                depressed,
                latched,
                locked,
                group,
            }) => WireEvent::Modifiers {
                depressed,
                latched,
                locked,
                group,
            },
        }
    }
}

impl From<WireEvent> for Event {
    fn from(event: WireEvent) -> Self {
        match event {
            WireEvent::Motion { dx, dy } => {
                Event::Pointer(PointerEvent::Motion { time: 0, dx, dy })
            }
            WireEvent::Button { button, state } => Event::Pointer(PointerEvent::Button {
                time: 0,
                button,
                state,
            }),
            WireEvent::Axis { axis, value } => Event::Pointer(PointerEvent::Axis {
                time: 0,
                axis,
                value,
            }),
            WireEvent::AxisDiscrete120 { axis, value } => {
                Event::Pointer(PointerEvent::AxisDiscrete120 { axis, value })
            }
            WireEvent::Key { key, state } => Event::Keyboard(KeyboardEvent::Key {
                time: 0,
                key,
                state,
            }),
            WireEvent::Modifiers {
                depressed,
                latched,
                locked,
                group,
            } => Event::Keyboard(KeyboardEvent::Modifiers {
                depressed,
                latched,
                locked,
                group,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_event_roundtrip() {
        let events = [
            Event::Pointer(PointerEvent::Motion {
                time: 0,
                dx: 1.5,
                dy: -3.0,
            }),
            Event::Pointer(PointerEvent::Button {
                time: 0,
                button: input_event::BTN_LEFT,
                state: 1,
            }),
            Event::Pointer(PointerEvent::Axis {
                time: 0,
                axis: 0,
                value: 2.0,
            }),
            Event::Pointer(PointerEvent::AxisDiscrete120 {
                axis: 1,
                value: -120,
            }),
            Event::Keyboard(KeyboardEvent::Key {
                time: 0,
                key: 30,
                state: 1,
            }),
            Event::Keyboard(KeyboardEvent::Modifiers {
                depressed: 1,
                latched: 2,
                locked: 3,
                group: 4,
            }),
        ];
        for event in events {
            let msg = Message::Input(event.into());
            let bytes = postcard::to_stdvec(&msg).unwrap();
            let decoded: Message = postcard::from_bytes(&bytes).unwrap();
            let Message::Input(wire) = decoded else {
                panic!("mauvais message")
            };
            assert_eq!(Event::from(wire), event);
        }
    }

    #[test]
    fn audio_roundtrip() {
        let frame = AudioFrame {
            rate: 48_000,
            channels: 2,
            samples: vec![0, -1, i16::MAX, i16::MIN],
        };
        let Message::Audio {
            kind,
            rate,
            channels,
            pcm,
        } = Message::audio(AudioKind::Mic, &frame)
        else {
            panic!("mauvais message")
        };
        assert_eq!(kind, AudioKind::Mic);
        assert_eq!(audio_frame(rate, channels, &pcm), Some(frame));
        assert_eq!(audio_frame(48_000, 2, &[0, 0, 0]), None);
        assert_eq!(audio_frame(48_000, 0, &[]), None);
    }
}

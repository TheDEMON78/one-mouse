//! Mise en paquets du son pour le réseau, en deux qualités :
//!
//! - **Sans perte** (par défaut) : flottants 32 bits, tels que capturés, en
//!   paquets de 2,5 ms (un datagramme UDP chacun, sans fragmentation). Un paquet
//!   perdu est comblé par le précédent.
//! - **Compressé** : Opus (le codec de Discord, Zoom, WebRTC) à 510 kbit/s en
//!   stéréo pour le son, 128 kbit/s pour le micro, trames de 10 ms à 48 kHz ;
//!   six fois moins de données, pour un Wi-Fi faible. Pertes reconstituées par
//!   Opus (correction d'erreurs pour le micro, masquage au-delà).

use anyhow::{Result, anyhow, ensure};
use opus::{Application, Bitrate, Channels, Decoder, Encoder};
use serde::{Deserialize, Serialize};

use crate::audio::{AudioFrame, Converter};
use crate::protocol::{AudioFormat, AudioKind};

/// Qualité de transport choisie par l'utilisateur.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum, Default)]
#[serde(rename_all = "lowercase")]
pub enum Quality {
    /// Son transmis sans perte (réseau local rapide, ~3 Mbit/s en stéréo).
    #[default]
    Lossless,
    /// Son compressé avec Opus (Wi-Fi faible, ~0,5 Mbit/s).
    Compressed,
}

impl Quality {
    pub const ALL: [Quality; 2] = [Quality::Lossless, Quality::Compressed];

    pub fn arg(self) -> &'static str {
        match self {
            Quality::Lossless => "lossless",
            Quality::Compressed => "compressed",
        }
    }
}

/// Fréquence d'Opus.
pub const OPUS_RATE: u32 = 48_000;
/// Trame Opus de 10 ms à 48 kHz, par canal.
const OPUS_FRAME: usize = 480;
/// Taille maximale d'un paquet Opus.
const MAX_OPUS_PACKET: usize = 4000;
/// Durée d'un paquet sans perte (µs) : 2,5 ms tiennent dans un datagramme.
const PCM_CHUNK_US: u32 = 2_500;
/// Au-delà de ce nombre de paquets perdus d'affilée, on ne masque plus.
const MAX_CONCEALED: u32 = 5;

fn opus_channels_for(kind: AudioKind) -> usize {
    match kind {
        AudioKind::Sound => 2,
        AudioKind::Mic => 1,
    }
}

fn opus_channels(count: usize) -> Channels {
    if count == 1 {
        Channels::Mono
    } else {
        Channels::Stereo
    }
}

/// Paquet prêt à envoyer.
pub struct Packet {
    pub seq: u32,
    pub format: AudioFormat,
    pub rate: u32,
    pub channels: u8,
    pub data: Vec<u8>,
}

pub struct AudioEncoder {
    inner: EncoderKind,
    seq: u32,
}

enum EncoderKind {
    Pcm {
        pending: Vec<f32>,
        rate: u32,
        channels: usize,
    },
    Opus {
        encoder: Box<Encoder>,
        converter: Box<Converter>,
        channels: usize,
        pending: Vec<f32>,
        buffer: Vec<u8>,
    },
}

impl AudioEncoder {
    pub fn new(kind: AudioKind, quality: Quality) -> Result<Self> {
        let inner = match quality {
            Quality::Lossless => EncoderKind::Pcm {
                pending: Vec::new(),
                rate: 0,
                channels: 0,
            },
            Quality::Compressed => {
                let channels = opus_channels_for(kind);
                let application = match kind {
                    AudioKind::Sound => Application::Audio,
                    AudioKind::Mic => Application::Voip,
                };
                let mut encoder = Encoder::new(OPUS_RATE, opus_channels(channels), application)?;
                let bitrate = match kind {
                    AudioKind::Sound => 510_000,
                    AudioKind::Mic => 128_000,
                };
                encoder.set_bitrate(Bitrate::Bits(bitrate))?;
                encoder.set_vbr(false)?;
                // La correction d'erreurs intégrée n'existe qu'en mode voix (SILK) :
                // l'activer pour la musique pousserait Opus vers ce mode.
                if kind == AudioKind::Mic {
                    encoder.set_inband_fec(true)?;
                    encoder.set_packet_loss_perc(5)?;
                }
                EncoderKind::Opus {
                    encoder: Box::new(encoder),
                    converter: Box::new(Converter::new(OPUS_RATE, channels as u16)),
                    channels,
                    pending: Vec::new(),
                    buffer: vec![0; MAX_OPUS_PACKET],
                }
            }
        };
        Ok(Self { inner, seq: 0 })
    }

    /// Ajoute du son capturé ; renvoie les paquets complets.
    pub fn push(&mut self, frame: &AudioFrame) -> Result<Vec<Packet>> {
        let mut out = Vec::new();
        match &mut self.inner {
            EncoderKind::Pcm {
                pending,
                rate,
                channels,
            } => {
                if frame.rate != *rate || frame.channels as usize != *channels {
                    pending.clear();
                    *rate = frame.rate;
                    *channels = frame.channels as usize;
                }
                pending.extend_from_slice(&frame.samples);
                let chunk = (*rate as u64 * PCM_CHUNK_US as u64 / 1_000_000) as usize * *channels;
                while chunk > 0 && pending.len() >= chunk {
                    let data = pending[..chunk]
                        .iter()
                        .flat_map(|s| s.to_le_bytes())
                        .collect();
                    pending.drain(..chunk);
                    out.push((AudioFormat::Pcm, *rate, *channels, data));
                }
            }
            EncoderKind::Opus {
                encoder,
                converter,
                channels,
                pending,
                buffer,
            } => {
                pending.extend(converter.convert(frame, 1.0));
                let len = OPUS_FRAME * *channels;
                while pending.len() >= len {
                    let size = encoder.encode_float(&pending[..len], buffer)?;
                    pending.drain(..len);
                    out.push((
                        AudioFormat::Opus,
                        OPUS_RATE,
                        *channels,
                        buffer[..size].to_vec(),
                    ));
                }
            }
        }
        Ok(out
            .into_iter()
            .map(|(format, rate, channels, data)| {
                let seq = self.seq;
                self.seq = self.seq.wrapping_add(1);
                Packet {
                    seq,
                    format,
                    rate,
                    channels: channels as u8,
                    data,
                }
            })
            .collect())
    }
}

pub struct AudioDecoder {
    format: AudioFormat,
    channels: usize,
    last_seq: Option<u32>,
    inner: DecoderKind,
}

enum DecoderKind {
    Pcm {
        /// Dernier paquet reçu, rejoué pour combler une perte.
        last: Option<AudioFrame>,
    },
    Opus {
        decoder: Box<Decoder>,
        buffer: Vec<f32>,
    },
}

impl AudioDecoder {
    pub fn new(format: AudioFormat, channels: u8) -> Result<Self> {
        let inner = match format {
            AudioFormat::Pcm => DecoderKind::Pcm { last: None },
            AudioFormat::Opus => DecoderKind::Opus {
                decoder: Box::new(Decoder::new(
                    OPUS_RATE,
                    opus_channels(channels.clamp(1, 2) as usize),
                )?),
                buffer: vec![0.0; OPUS_FRAME * 6 * 2],
            },
        };
        let channels = match format {
            AudioFormat::Pcm => channels as usize,
            AudioFormat::Opus => channels.clamp(1, 2) as usize,
        };
        Ok(Self {
            format,
            channels,
            last_seq: None,
            inner,
        })
    }

    /// Ce décodeur convient-il à ces paquets ?
    pub fn matches(&self, format: AudioFormat, channels: u8) -> bool {
        self.format == format && self.channels == channels as usize
    }

    /// Décode un paquet ; comble les paquets manquants juste avant lui.
    /// Renvoie `None` pour un paquet en double ou en retard.
    pub fn decode(&mut self, seq: u32, rate: u32, data: &[u8]) -> Result<Option<AudioFrame>> {
        let missing = match self.last_seq {
            Some(last) => {
                let gap = seq.wrapping_sub(last);
                if gap == 0 || gap > u32::MAX / 2 {
                    return Ok(None);
                }
                gap - 1
            }
            None => 0,
        };
        self.last_seq = Some(seq);
        let conceal = (1..=MAX_CONCEALED).contains(&missing);
        let channels = self.channels;
        match &mut self.inner {
            DecoderKind::Pcm { last } => {
                ensure!(
                    (8_000..=192_000).contains(&rate)
                        && (1..=8).contains(&channels)
                        && data.len().is_multiple_of(4 * channels),
                    "paquet audio invalide"
                );
                let current: Vec<f32> = data
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|&b| f32::from_le_bytes(b))
                    .collect();
                ensure!(
                    current.iter().all(|s| s.is_finite()),
                    "paquet audio invalide"
                );
                let mut samples = Vec::with_capacity(current.len() * (missing as usize + 1));
                if conceal && let Some(previous) = last.as_ref().filter(|p| p.rate == rate) {
                    for _ in 0..missing {
                        samples.extend_from_slice(&previous.samples);
                    }
                }
                samples.extend_from_slice(&current);
                *last = Some(AudioFrame {
                    rate,
                    channels: channels as u16,
                    samples: current,
                });
                Ok(Some(AudioFrame {
                    rate,
                    channels: channels as u16,
                    samples,
                }))
            }
            DecoderKind::Opus { decoder, buffer } => {
                let mut run = |data: &[u8], fec: bool| -> Result<Vec<f32>> {
                    // Masquage et correction d'erreurs : Opus rend exactement la durée
                    // du tampon fourni, qui doit donc faire une trame (10 ms).
                    let len = if data.is_empty() || fec {
                        OPUS_FRAME * channels
                    } else {
                        buffer.len()
                    };
                    let per_channel = decoder
                        .decode_float(data, &mut buffer[..len], fec)
                        .map_err(|e| anyhow!("décodage Opus : {e}"))?;
                    Ok(buffer[..per_channel * channels].to_vec())
                };
                let mut samples = Vec::new();
                if conceal {
                    // Le dernier paquet perdu est retrouvé grâce à la correction
                    // d'erreurs portée par le paquet courant, les autres sont masqués.
                    for _ in 1..missing {
                        samples.extend(run(&[], false)?);
                    }
                    samples.extend(run(data, true)?);
                }
                samples.extend(run(data, false)?);
                Ok(Some(AudioFrame {
                    rate: OPUS_RATE,
                    channels: channels as u16,
                    samples,
                }))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f32, start: usize, len: usize, rate: u32, channels: usize) -> AudioFrame {
        let samples = (start..start + len)
            .flat_map(|i| {
                let v = 0.5 * (2.0 * std::f32::consts::PI * freq * i as f32 / rate as f32).sin();
                std::iter::repeat_n(v, channels)
            })
            .collect();
        AudioFrame {
            rate,
            channels: channels as u16,
            samples,
        }
    }

    /// Rapport signal/bruit entre un sinus pur et le son décodé (en dB).
    fn snr(decoded: &[f32], freq: f64, rate: f64) -> f64 {
        let n = decoded.len();
        let (mut ss, mut sc) = (0.0, 0.0);
        for (i, &v) in decoded.iter().enumerate() {
            let w = 2.0 * std::f64::consts::PI * freq * i as f64 / rate;
            ss += v as f64 * w.sin();
            sc += v as f64 * w.cos();
        }
        let (a, b) = (2.0 * ss / n as f64, 2.0 * sc / n as f64);
        let mut noise = 0.0;
        let mut signal = 0.0;
        for (i, &v) in decoded.iter().enumerate() {
            let w = 2.0 * std::f64::consts::PI * freq * i as f64 / rate;
            let fit = a * w.sin() + b * w.cos();
            signal += fit * fit;
            noise += (v as f64 - fit).powi(2);
        }
        10.0 * (signal / noise).log10()
    }

    #[test]
    fn music_quality_is_transparent() {
        let mut enc = AudioEncoder::new(AudioKind::Sound, Quality::Compressed).unwrap();
        let mut dec = AudioDecoder::new(AudioFormat::Opus, 2).unwrap();
        let mut left = Vec::new();
        for k in 0..100 {
            for p in enc.push(&sine(1000.0, k * 480, 480, OPUS_RATE, 2)).unwrap() {
                assert!(
                    p.data.len() < 1400,
                    "un paquet par datagramme : {}",
                    p.data.len()
                );
                let frame = dec.decode(p.seq, p.rate, &p.data).unwrap().unwrap();
                left.extend(frame.samples.iter().step_by(2));
            }
        }
        // On ignore le démarrage du codec.
        let quality = snr(&left[4800..], 1000.0, 48_000.0);
        assert!(quality > 50.0, "rapport signal/bruit {quality:.1} dB");
    }

    #[test]
    fn quality_holds_with_44k_capture() {
        let mut enc = AudioEncoder::new(AudioKind::Sound, Quality::Compressed).unwrap();
        let mut dec = AudioDecoder::new(AudioFormat::Opus, 2).unwrap();
        let mut left = Vec::new();
        for k in 0..100 {
            for p in enc.push(&sine(1000.0, k * 441, 441, 44_100, 2)).unwrap() {
                let frame = dec.decode(p.seq, p.rate, &p.data).unwrap().unwrap();
                left.extend(frame.samples.iter().step_by(2));
            }
        }
        let quality = snr(&left[4800..], 1000.0, 48_000.0);
        assert!(quality > 50.0, "rapport signal/bruit {quality:.1} dB");
    }

    #[test]
    fn capture_rate_is_converted_to_48k() {
        let mut enc = AudioEncoder::new(AudioKind::Mic, Quality::Compressed).unwrap();
        let mut count = 0;
        for k in 0..100 {
            count += enc
                .push(&sine(440.0, k * 441, 441, 44_100, 2))
                .unwrap()
                .len();
        }
        // 1 s à 44,1 kHz stéréo = ~100 paquets de 10 ms mono à 48 kHz.
        assert!((98..=100).contains(&count), "{count}");
    }

    #[test]
    fn lost_packets_are_concealed_and_late_ones_dropped() {
        let mut enc = AudioEncoder::new(AudioKind::Sound, Quality::Compressed).unwrap();
        let mut dec = AudioDecoder::new(AudioFormat::Opus, 2).unwrap();
        let packets: Vec<Packet> = (0..10)
            .flat_map(|k| enc.push(&sine(500.0, k * 480, 480, OPUS_RATE, 2)).unwrap())
            .collect();
        let first = dec
            .decode(packets[0].seq, packets[0].rate, &packets[0].data)
            .unwrap()
            .unwrap();
        assert_eq!(first.samples.len(), 960);
        // Paquets 1 et 2 perdus : 3 trames rendues (2 reconstituées + la courante).
        let third = dec
            .decode(packets[3].seq, packets[3].rate, &packets[3].data)
            .unwrap()
            .unwrap();
        assert_eq!(third.samples.len(), 3 * 960);
        // Le paquet 2 arrive trop tard : ignoré.
        assert!(
            dec.decode(packets[2].seq, packets[2].rate, &packets[2].data)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn lossless_is_bit_exact_and_fits_a_datagram() {
        let mut enc = AudioEncoder::new(AudioKind::Sound, Quality::Lossless).unwrap();
        let mut dec = AudioDecoder::new(AudioFormat::Pcm, 2).unwrap();
        let input = sine(1000.0, 0, 4410, 44_100, 2);
        let mut output = Vec::new();
        for p in enc.push(&input).unwrap() {
            assert!(
                p.data.len() <= 1200,
                "un datagramme sans fragmentation : {}",
                p.data.len()
            );
            output.extend(dec.decode(p.seq, p.rate, &p.data).unwrap().unwrap().samples);
        }
        assert_eq!(output, input.samples[..output.len()]);
        assert!(output.len() >= input.samples.len() - 2 * 110);
    }

    #[test]
    fn lossless_conceals_a_lost_packet() {
        let mut enc = AudioEncoder::new(AudioKind::Sound, Quality::Lossless).unwrap();
        let mut dec = AudioDecoder::new(AudioFormat::Pcm, 2).unwrap();
        let packets = enc.push(&sine(1000.0, 0, 4800, 48_000, 2)).unwrap();
        let first = dec
            .decode(packets[0].seq, packets[0].rate, &packets[0].data)
            .unwrap()
            .unwrap();
        // Paquet 1 perdu : le paquet 0 est rejoué à sa place, la durée est conservée.
        let third = dec
            .decode(packets[2].seq, packets[2].rate, &packets[2].data)
            .unwrap()
            .unwrap();
        assert_eq!(third.samples.len(), 2 * first.samples.len());
        assert!(
            dec.decode(packets[1].seq, packets[1].rate, &packets[1].data)
                .unwrap()
                .is_none()
        );
    }
}

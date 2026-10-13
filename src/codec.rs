//! Mise en paquets du son pour le réseau, en deux qualités :
//!
//! - **Sans perte** (par défaut) : entiers 24 bits (qualité studio, 138 dB
//!   de dynamique avec une marge de 6 dB au-dessus du maximum), en paquets de
//!   2 ms (un datagramme UDP chacun, sans fragmentation). Chaque
//!   paquet transporte aussi le précédent ; si les deux sont perdus, le trou est
//!   comblé en prolongeant le son reçu juste avant, période après période.
//! - **Compressé** : Opus (le codec de Discord, Zoom, WebRTC) à 510 kbit/s en
//!   stéréo pour le son, 128 kbit/s pour le micro, trames de 10 ms à 48 kHz ;
//!   six fois moins de données, pour un Wi-Fi faible. Pertes reconstituées par
//!   Opus (correction d'erreurs pour le micro, masquage au-delà).

use anyhow::{Result, anyhow, ensure};
use opus::{Application, Bitrate, Channels, Decoder, Encoder};
use serde::{Deserialize, Serialize};

use crate::audio::{AudioFrame, Converter, remix};
use crate::protocol::{AudioFormat, AudioKind};

/// Qualité de transport choisie par l'utilisateur.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum, Default)]
#[serde(rename_all = "lowercase")]
pub enum Quality {
    /// Son transmis sans perte audible, 24 bits (réseau local, ~4,6 Mbit/s en stéréo).
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
/// Durée maximale d'un paquet sans perte (µs).
const PCM_CHUNK_US: u32 = 2_500;
/// Taille maximale d'un paquet sans perte : avec la copie du paquet précédent
/// (contre les pertes), un datagramme reste sous 1 200 octets, sans fragmentation
/// IP, même à travers un VPN (2 ms en stéréo 48 kHz).
const PCM_MAX_BYTES: usize = 576;
/// Octets par échantillon sans perte (24 bits).
const PCM_SAMPLE_BYTES: usize = 3;
/// Échelle des entiers 24 bits : ±2 représentables (6 dB de marge au-dessus du
/// maximum, un mélange de sons peut le dépasser), pas de 2⁻²² (−132 dB).
const PCM_SCALE: f32 = (1 << 22) as f32;
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
        /// Canaux de la capture.
        source: usize,
        /// Canaux envoyés : stéréo au plus pour le son, mono pour le micro.
        channels: usize,
        max_channels: usize,
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
    #[cfg(test)]
    pub fn new(kind: AudioKind, quality: Quality) -> Result<Self> {
        Self::for_link(kind, quality, false)
    }

    /// `bluetooth` : liaison Bluetooth (1 à 2 Mbit/s en tout). Le son y est
    /// toujours compressé, à un débit qui laisse de la marge à la souris :
    /// 128 kbit/s en stéréo et 48 kbit/s pour le micro (très bonne qualité Opus).
    pub fn for_link(kind: AudioKind, quality: Quality, bluetooth: bool) -> Result<Self> {
        let quality = if bluetooth {
            Quality::Compressed
        } else {
            quality
        };
        let inner = match quality {
            Quality::Lossless => EncoderKind::Pcm {
                pending: Vec::new(),
                rate: 0,
                source: 0,
                channels: 0,
                max_channels: opus_channels_for(kind),
            },
            Quality::Compressed => {
                let channels = opus_channels_for(kind);
                let application = match kind {
                    AudioKind::Sound => Application::Audio,
                    AudioKind::Mic => Application::Voip,
                };
                let mut encoder = Encoder::new(OPUS_RATE, opus_channels(channels), application)?;
                let bitrate = match (kind, bluetooth) {
                    (AudioKind::Sound, false) => 510_000,
                    (AudioKind::Mic, false) => 128_000,
                    (AudioKind::Sound, true) => 128_000,
                    (AudioKind::Mic, true) => 48_000,
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
                source,
                channels,
                max_channels,
            } => {
                if frame.rate != *rate || frame.channels as usize != *source {
                    pending.clear();
                    *rate = frame.rate;
                    *source = frame.channels as usize;
                    *channels = (*source).min(*max_channels);
                }
                pending.extend(remix(&frame.samples, *source, *channels));
                let frames = ((*rate as u64 * PCM_CHUNK_US as u64 / 1_000_000) as usize)
                    .min(PCM_MAX_BYTES / (PCM_SAMPLE_BYTES * (*channels).max(1)));
                let chunk = frames * *channels;
                while chunk > 0 && pending.len() >= chunk {
                    let data = pending[..chunk].iter().flat_map(|&s| pcm24(s)).collect();
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
        /// Son reçu juste avant (entrelacé), pour combler les pertes.
        history: Vec<f32>,
        history_rate: u32,
    },
    Opus {
        decoder: Box<Decoder>,
        buffer: Vec<f32>,
    },
}

impl AudioDecoder {
    pub fn new(format: AudioFormat, channels: u8) -> Result<Self> {
        let inner = match format {
            AudioFormat::Pcm => DecoderKind::Pcm {
                history: Vec::new(),
                history_rate: 0,
            },
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
    #[cfg(test)]
    pub fn decode(&mut self, seq: u32, rate: u32, data: &[u8]) -> Result<Option<AudioFrame>> {
        self.decode_with(seq, rate, data, &[])
    }

    /// Décode le paquet `seq`. `previous` est la copie du paquet `seq - 1` qu'il
    /// transporte (vide si aucune) : s'il manque, il est restitué tel quel.
    pub fn decode_with(
        &mut self,
        seq: u32,
        rate: u32,
        data: &[u8],
        previous: &[u8],
    ) -> Result<Option<AudioFrame>> {
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
            DecoderKind::Pcm {
                history,
                history_rate,
            } => {
                ensure!(
                    (8_000..=192_000).contains(&rate) && (1..=8).contains(&channels),
                    "paquet audio invalide"
                );
                let current = parse_pcm(data, channels)?;
                // Paquet précédent perdu mais transporté par celui-ci : restitué exactement.
                let recovered = if conceal && !previous.is_empty() {
                    parse_pcm(previous, channels).ok()
                } else {
                    None
                };
                // Son réel qui suit le trou : copie restituée puis paquet courant.
                let mut next = recovered.unwrap_or_default();
                next.extend_from_slice(&current);
                let mut samples = Vec::with_capacity(current.len() * (missing as usize + 1));
                if *history_rate != rate {
                    history.clear();
                    *history_rate = rate;
                }
                if conceal {
                    let lost = (missing - u32::from(next.len() > current.len())) as usize;
                    let frames = lost * (current.len() / channels);
                    samples.extend(conceal_pcm(history, channels, rate, frames, &mut next));
                }
                samples.extend(next);
                history.extend_from_slice(&samples);
                let keep = (rate * CONCEAL_HISTORY_MS / 1000) as usize * channels;
                if history.len() > keep {
                    history.drain(..history.len() - keep);
                }
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
                    // Le dernier paquet perdu est restitué : copie transportée par le
                    // paquet courant, sinon sa correction d'erreurs. Les autres sont masqués.
                    for _ in 1..missing {
                        samples.extend(run(&[], false)?);
                    }
                    match previous.is_empty() {
                        false => samples.extend(run(previous, false)?),
                        true => samples.extend(run(data, true)?),
                    }
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

/// Trames de fondu entre le son de remplacement et le son réel qui revient.
const CONCEAL_FADE: usize = 32;
/// Son récent gardé pour combler les pertes (ms).
const CONCEAL_HISTORY_MS: u32 = 20;

/// Comble `frames` trames perdues à la suite de `history` (son reçu juste
/// avant, entrelacé) : la période du son est cherchée dans `history`, puis le
/// son est prolongé période après période, sans saut ; `next` (le son réel qui
/// revient) est rejoint par un fondu. Méthode des téléphones numériques
/// (G.711 annexe I). Une simple répétition du dernier paquet faisait deux
/// sauts, audibles comme un grésillement.
fn conceal_pcm(
    history: &[f32],
    channels: usize,
    rate: u32,
    frames: usize,
    next: &mut [f32],
) -> Vec<f32> {
    let channels = channels.max(1);
    let len = history.len() / channels;
    if len < 4 || frames == 0 {
        return Vec::new();
    }
    let period = find_period(history, channels, rate);
    let start = len - period;
    let frame_at = |n: usize| start + n % period;
    let mut out = Vec::with_capacity(frames * channels);
    for n in 0..frames {
        let f = frame_at(n);
        out.extend_from_slice(&history[f * channels..(f + 1) * channels]);
    }
    let fade = CONCEAL_FADE.min(next.len() / channels);
    for k in 0..fade {
        let f = frame_at(frames + k);
        let w = (k + 1) as f32 / (fade + 1) as f32;
        for c in 0..channels {
            let ghost = history[f * channels + c];
            let real = &mut next[k * channels + c];
            *real = ghost + (*real - ghost) * w;
        }
    }
    out
}

/// Période (en trames) qui ressemble le plus à la fin de `history` : maximum
/// de corrélation normalisée entre la fin du son et le son décalé. Entre
/// 0,5 ms et 15 ms ; à défaut, tout l'historique.
fn find_period(history: &[f32], channels: usize, rate: u32) -> usize {
    let len = history.len() / channels;
    let mono: Vec<f32> = history
        .chunks_exact(channels)
        .map(|f| f.iter().sum::<f32>() / channels as f32)
        .collect();
    let window = (rate as usize / 400).clamp(8, len / 3);
    let min = (rate as usize / 2000).max(2);
    let max = (rate as usize * 15 / 1000).min(len - window);
    if min >= max {
        return len;
    }
    let target = &mono[len - window..];
    let mut best = (f32::MIN, len);
    for period in min..=max {
        let candidate = &mono[len - window - period..len - period];
        let (mut dot, mut energy) = (0.0f32, 0.0f32);
        for (a, b) in target.iter().zip(candidate) {
            dot += a * b;
            energy += b * b;
        }
        let score = if energy > 1e-12 {
            dot / energy.sqrt()
        } else {
            0.0
        };
        if score > best.0 {
            best = (score, period);
        }
    }
    best.1
}

/// Échantillons d'un paquet sans perte (entiers 24 bits little-endian).
fn parse_pcm(data: &[u8], channels: usize) -> Result<Vec<f32>> {
    ensure!(
        data.len().is_multiple_of(PCM_SAMPLE_BYTES * channels),
        "paquet audio invalide"
    );
    Ok(data
        .as_chunks::<PCM_SAMPLE_BYTES>()
        .0
        .iter()
        .map(|&[a, b, c]| (i32::from_le_bytes([0, a, b, c]) >> 8) as f32 / PCM_SCALE)
        .collect())
}

/// Échantillon en entier 24 bits little-endian.
fn pcm24(sample: f32) -> [u8; PCM_SAMPLE_BYTES] {
    let max = (1 << 23) - 1;
    let value = if sample.is_finite() {
        ((sample * PCM_SCALE).round() as i32).clamp(-max - 1, max)
    } else {
        0
    };
    let [a, b, c, _] = value.to_le_bytes();
    [a, b, c]
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

    /// Écart maximal du transport sans perte : un pas de l'échelle 24 bits.
    fn assert_lossless(output: &[f32], input: &[f32]) {
        assert!(!output.is_empty());
        for (o, i) in output.iter().zip(input) {
            assert!(
                (o - i).abs() <= 0.5 / PCM_SCALE + f32::EPSILON,
                "{o} au lieu de {i}"
            );
        }
    }

    #[test]
    fn lossless_is_studio_quality() {
        // Maximum dépassé (mélange de sons) : conservé grâce à la marge.
        for v in [0.0, 1.0, -1.0, 0.123_456_7, 1.9, -1.99] {
            let back = parse_pcm(&pcm24(v), 1).unwrap()[0];
            assert!((back - v).abs() <= 0.5 / PCM_SCALE, "{v} -> {back}");
        }
        assert_eq!(parse_pcm(&pcm24(f32::NAN), 1).unwrap()[0], 0.0);
    }

    #[test]
    fn lossless_is_exact_and_fits_a_datagram() {
        let mut enc = AudioEncoder::new(AudioKind::Sound, Quality::Lossless).unwrap();
        let mut dec = AudioDecoder::new(AudioFormat::Pcm, 2).unwrap();
        let input = sine(1000.0, 0, 4410, 44_100, 2);
        let mut output = Vec::new();
        for p in enc.push(&input).unwrap() {
            // Avec la copie du paquet précédent, le datagramme chiffré reste sous
            // la taille d'un paquet IP (1 472 octets utiles) : pas de fragmentation.
            let msg = crate::protocol::Message::Audio {
                kind: AudioKind::Sound,
                seq: p.seq,
                format: p.format,
                rate: p.rate,
                channels: p.channels,
                data: p.data.clone(),
                previous: p.data.clone(),
            };
            let datagram = postcard::to_stdvec(&msg).unwrap().len() + 8 + 16;
            assert!(datagram <= 1200, "datagramme de {datagram} octets");
            output.extend(dec.decode(p.seq, p.rate, &p.data).unwrap().unwrap().samples);
        }
        assert_lossless(&output, &input.samples);
        assert!(output.len() >= input.samples.len() - 2 * 110);
    }

    #[test]
    fn lost_packet_is_restored_exactly_from_the_next_one() {
        let mut enc = AudioEncoder::new(AudioKind::Sound, Quality::Lossless).unwrap();
        let mut dec = AudioDecoder::new(AudioFormat::Pcm, 2).unwrap();
        let input = sine(1000.0, 0, 4800, 48_000, 2);
        let packets = enc.push(&input).unwrap();
        let mut output = Vec::new();
        for (i, p) in packets.iter().enumerate() {
            // Un paquet sur cinq perdu ; chacun transporte la copie du précédent.
            if i % 5 == 3 {
                continue;
            }
            let previous = if i > 0 { &packets[i - 1].data[..] } else { &[] };
            let frame = dec.decode_with(p.seq, p.rate, &p.data, previous).unwrap();
            output.extend(frame.unwrap().samples);
        }
        // Son identique, sans trou.
        assert_lossless(&output, &input.samples);
        assert_eq!(output.len(), packets.len() * packets[0].data.len() / 3);
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

    /// Deux paquets perdus d'affilée (le second restitué par sa copie, le premier
    /// comblé) : le son reste continu, sans saut audible.
    #[test]
    fn lost_packets_are_concealed_smoothly() {
        let mut enc = AudioEncoder::new(AudioKind::Sound, Quality::Lossless).unwrap();
        let mut packets = Vec::new();
        for k in 0..40 {
            packets.extend(enc.push(&sine(1000.0, k * 240, 240, 48_000, 1)).unwrap());
        }
        let mut dec = AudioDecoder::new(AudioFormat::Pcm, 1).unwrap();
        let mut out = Vec::new();
        let mut previous: Vec<u8> = Vec::new();
        for p in &packets {
            let prev = std::mem::replace(&mut previous, p.data.clone());
            if p.seq == 30 || p.seq == 31 {
                continue;
            }
            if let Some(f) = dec.decode_with(p.seq, p.rate, &p.data, &prev).unwrap() {
                out.extend(f.samples);
            }
        }
        // Rien ne manque, et pas de saut : différence seconde d'un sinus de 1 kHz
        // d'amplitude 0,5 de 0,0086 au plus (une répétition brute donnait 0,13).
        assert_eq!(out.len(), packets.len() * packets[0].data.len() / 3);
        let worst = out
            .windows(3)
            .map(|w| (w[0] - 2.0 * w[1] + w[2]).abs())
            .fold(0.0, f32::max);
        assert!(worst < 0.012, "saut de {worst}");
    }
}

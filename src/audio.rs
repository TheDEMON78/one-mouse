//! Partage du son entre les deux PC.
//!
//! - Capture : son du système (ce qui sort des haut-parleurs) ou micro.
//!   Le son du système est capturé en « loopback » sous Windows et macOS (14.6+),
//!   et via la source « monitor » de PulseAudio / PipeWire sous Linux.
//! - Lecture : le son reçu est converti (fréquence, canaux) pour la sortie choisie,
//!   avec un petit tampon pour absorber les variations du réseau.
//!
//! Les flux cpal ne sont pas `Send` sur toutes les plateformes : chacun vit dans
//! son propre thread, arrêté quand l'objet Rust correspondant est détruit.

use std::collections::VecDeque;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{
    BufferSize, Device, FromSample, Host, Sample, SampleFormat, SizedSample, StreamConfig,
    SupportedBufferSize, SupportedStreamConfig,
};
use tokio::sync::mpsc;

/// Durée d'un paquet audio envoyé sur le réseau.
const FRAME_MS: u32 = 10;
/// Son accumulé avant de démarrer (ou redémarrer) la lecture.
const PRIME_MS: u32 = 60;
/// Au-delà de ce retard, le son en trop est jeté pour rester synchrone.
const MAX_BUFFER_MS: u32 = 200;

/// Quand partager le son du système ou le micro.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Mode {
    /// Jamais.
    Off,
    /// Son : quand l'autre PC contrôle celui-ci. Micro : quand ce PC contrôle l'autre.
    Auto,
    /// Tout le temps, tant que les deux PC sont connectés.
    Always,
}

impl Mode {
    pub fn wanted(self, active: bool) -> bool {
        match self {
            Mode::Off => false,
            Mode::Auto => active,
            Mode::Always => true,
        }
    }
}

/// Réglages audio choisis en ligne de commande.
#[derive(Debug, Clone)]
pub struct AudioConfig {
    pub sound: Mode,
    pub mic: Mode,
    /// Source du son du système (par défaut : la sortie audio principale).
    pub sound_device: Option<String>,
    /// Micro à utiliser (par défaut : le micro principal).
    pub mic_device: Option<String>,
    /// Sortie où jouer le son reçu de l'autre PC.
    pub sound_output: Option<String>,
    /// Sortie où jouer le micro reçu de l'autre PC (ex. un câble audio virtuel).
    pub mic_output: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Ce qui sort des haut-parleurs du PC.
    System,
    /// Le micro.
    Microphone,
}

/// Paquet de son PCM 16 bits entrelacé.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioFrame {
    pub rate: u32,
    pub channels: u16,
    pub samples: Vec<i16>,
}

impl AudioFrame {
    pub fn is_valid(&self) -> bool {
        (8_000..=192_000).contains(&self.rate)
            && (1..=8).contains(&self.channels)
            && self.samples.len().is_multiple_of(self.channels as usize)
    }
}

fn host() -> Host {
    // Sous Linux, PulseAudio (ou PipeWire en compatibilité) donne accès aux sources « monitor ».
    #[cfg(target_os = "linux")]
    if let Ok(host) = cpal::host_from_id(cpal::HostId::PulseAudio) {
        return host;
    }
    cpal::default_host()
}

fn device_label(device: &Device) -> String {
    match (device.description(), device.id()) {
        (Ok(desc), _) => desc.name().to_owned(),
        (_, Ok(id)) => id.id().to_owned(),
        _ => "?".to_owned(),
    }
}

/// Trouve un périphérique par son nom (même partiel, sans tenir compte de la casse) ou son identifiant.
fn find_device(devices: impl Iterator<Item = Device>, wanted: &str) -> Option<Device> {
    let wanted_lower = wanted.to_lowercase();
    devices.into_iter().find(|device| {
        let id_matches = device
            .id()
            .is_ok_and(|id| id.id() == wanted || id.to_string() == wanted);
        id_matches || device_label(device).to_lowercase().contains(&wanted_lower)
    })
}

/// Affiche les périphériques audio disponibles.
pub fn list_devices() -> Result<()> {
    let host = host();
    println!("Système audio : {}", host.id().name());
    println!("\nEntrées (micros, sources) :");
    for device in host.input_devices()? {
        println!("  - {}  [{}]", device_label(&device), id_string(&device));
    }
    println!("\nSorties (haut-parleurs, câbles virtuels) :");
    for device in host.output_devices()? {
        println!("  - {}  [{}]", device_label(&device), id_string(&device));
    }
    Ok(())
}

fn id_string(device: &Device) -> String {
    device.id().map(|id| id.id().to_owned()).unwrap_or_default()
}

fn capture_device(
    host: &Host,
    source: Source,
    wanted: Option<&str>,
) -> Result<(Device, SupportedStreamConfig)> {
    if let Some(wanted) = wanted {
        if let Some(device) = find_device(host.input_devices()?, wanted) {
            let config = device.default_input_config()?;
            return Ok((device, config));
        }
        // Une sortie peut aussi être capturée en loopback (Windows, macOS).
        if source == Source::System
            && let Some(device) = find_device(host.output_devices()?, wanted)
        {
            let config = device.default_output_config()?;
            return Ok((device, config));
        }
        bail!("périphérique audio « {wanted} » introuvable (voir `one-mouse audio-devices`)");
    }
    match source {
        Source::Microphone => {
            let device = host.default_input_device().context("aucun micro trouvé")?;
            let config = device.default_input_config()?;
            Ok((device, config))
        }
        Source::System => system_loopback(host),
    }
}

#[cfg(target_os = "linux")]
fn system_loopback(host: &Host) -> Result<(Device, SupportedStreamConfig)> {
    let sink = host
        .default_output_device()
        .context("aucune sortie audio trouvée")?
        .id()?;
    let monitor = format!("{}.monitor", sink.id());
    let device = host
        .input_devices()?
        .find(|d| d.id().is_ok_and(|id| id.id() == monitor))
        .with_context(|| {
            format!("source « {monitor} » introuvable (PulseAudio / PipeWire requis)")
        })?;
    let config = device.default_input_config()?;
    Ok((device, config))
}

#[cfg(not(target_os = "linux"))]
fn system_loopback(host: &Host) -> Result<(Device, SupportedStreamConfig)> {
    // Windows (WASAPI) et macOS 14.6+ : un flux d'entrée sur une sortie capture ce qu'elle joue.
    let device = host
        .default_output_device()
        .context("aucune sortie audio trouvée")?;
    let config = device.default_output_config()?;
    Ok((device, config))
}

/// Capture audio en cours. S'arrête quand l'objet est détruit.
pub struct Capture {
    pub frames: mpsc::Receiver<AudioFrame>,
    _stop: std_mpsc::Sender<()>,
}

impl Capture {
    pub fn start(source: Source, device: Option<&str>) -> Result<Self> {
        let device = device.map(str::to_owned);
        let (frames_tx, frames) = mpsc::channel(64);
        let (stop_tx, stop_rx) = std_mpsc::channel::<()>();
        let (ready_tx, ready_rx) = std_mpsc::channel::<Result<String>>();
        std::thread::Builder::new()
            .name("capture-audio".into())
            .spawn(move || {
                let started = (|| {
                    let host = host();
                    let (device, config) = capture_device(&host, source, device.as_deref())?;
                    let stream = build_input(&device, &config, frames_tx)?;
                    stream.play()?;
                    Ok((stream, device_label(&device)))
                })();
                match started {
                    Ok((stream, label)) => {
                        let _ = ready_tx.send(Ok(label));
                        // Attend l'arrêt (message ou destruction de `Capture`).
                        let _ = stop_rx.recv();
                        drop(stream);
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                    }
                }
            })?;
        let label = ready_rx
            .recv()
            .map_err(|_| anyhow!("le thread audio s'est arrêté"))??;
        log::info!("capture audio : {label}");
        Ok(Self {
            frames,
            _stop: stop_tx,
        })
    }
}

/// Configuration à faible latence : tampon de ~10 ms au lieu de la valeur par défaut,
/// qui atteint plusieurs secondes avec PulseAudio.
fn low_latency(config: &SupportedStreamConfig) -> StreamConfig {
    let mut stream_config = config.config();
    let wanted = stream_config.sample_rate * FRAME_MS / 1000;
    stream_config.buffer_size = match *config.buffer_size() {
        SupportedBufferSize::Range { min, max } => BufferSize::Fixed(wanted.clamp(min, max)),
        SupportedBufferSize::Unknown => BufferSize::Fixed(wanted),
    };
    stream_config
}

/// Essaie d'abord un petit tampon, puis la configuration par défaut du système.
fn with_fallback(
    config: &SupportedStreamConfig,
    mut build: impl FnMut(StreamConfig) -> Result<cpal::Stream>,
) -> Result<cpal::Stream> {
    build(low_latency(config)).or_else(|e| {
        log::debug!("tampon audio court refusé ({e:#}), configuration par défaut");
        build(config.config())
    })
}

fn build_input(
    device: &Device,
    config: &SupportedStreamConfig,
    tx: mpsc::Sender<AudioFrame>,
) -> Result<cpal::Stream> {
    with_fallback(config, |stream_config| {
        build_input_format(device, config.sample_format(), stream_config, tx.clone())
    })
}

fn build_input_format(
    device: &Device,
    format: SampleFormat,
    stream_config: StreamConfig,
    tx: mpsc::Sender<AudioFrame>,
) -> Result<cpal::Stream> {
    match format {
        SampleFormat::F32 => build_input_typed::<f32>(device, stream_config, tx),
        SampleFormat::F64 => build_input_typed::<f64>(device, stream_config, tx),
        SampleFormat::I16 => build_input_typed::<i16>(device, stream_config, tx),
        SampleFormat::I32 => build_input_typed::<i32>(device, stream_config, tx),
        SampleFormat::U16 => build_input_typed::<u16>(device, stream_config, tx),
        SampleFormat::U8 => build_input_typed::<u8>(device, stream_config, tx),
        other => bail!("format audio non géré : {other}"),
    }
}

fn build_input_typed<T>(
    device: &Device,
    config: StreamConfig,
    tx: mpsc::Sender<AudioFrame>,
) -> Result<cpal::Stream>
where
    T: SizedSample,
    i16: FromSample<T>,
{
    let rate = config.sample_rate;
    let channels = config.channels;
    let frame_len = (rate * FRAME_MS / 1000) as usize * channels as usize;
    let mut buffer = Vec::with_capacity(frame_len);
    let stream = device.build_input_stream(
        config,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            for &sample in data {
                buffer.push(sample.to_sample::<i16>());
                if buffer.len() >= frame_len {
                    let samples = std::mem::replace(&mut buffer, Vec::with_capacity(frame_len));
                    // Réseau saturé : on perd ce paquet plutôt que d'accumuler du retard.
                    let _ = tx.try_send(AudioFrame {
                        rate,
                        channels,
                        samples,
                    });
                }
            }
        },
        |e| log::warn!("capture audio : {e}"),
        None,
    )?;
    Ok(stream)
}

/// Tampon partagé entre le réseau et le thread de lecture.
struct PlayBuffer {
    samples: VecDeque<f32>,
    primed: bool,
    prime_len: usize,
    max_len: usize,
    channels: usize,
}

impl PlayBuffer {
    fn push(&mut self, samples: &[f32]) {
        self.samples.extend(samples);
        if self.samples.len() > self.max_len {
            // Trop de retard : on ne garde que la fin, en restant aligné sur les canaux.
            let excess = self.samples.len() - self.prime_len;
            let excess = excess - excess % self.channels;
            self.samples.drain(..excess);
        }
    }

    fn pop(&mut self) -> f32 {
        if !self.primed {
            if self.samples.len() < self.prime_len {
                return 0.0;
            }
            self.primed = true;
        }
        match self.samples.pop_front() {
            Some(s) => s,
            None => {
                self.primed = false;
                0.0
            }
        }
    }
}

/// Lecture du son reçu de l'autre PC.
pub struct Player {
    buffer: Arc<Mutex<PlayBuffer>>,
    converter: Converter,
    _stop: std_mpsc::Sender<()>,
}

impl Player {
    pub fn start(device: Option<&str>) -> Result<Self> {
        let device = device.map(str::to_owned);
        let (stop_tx, stop_rx) = std_mpsc::channel::<()>();
        let (ready_tx, ready_rx) =
            std_mpsc::channel::<Result<(Arc<Mutex<PlayBuffer>>, u32, u16, String)>>();
        std::thread::Builder::new()
            .name("lecture-audio".into())
            .spawn(move || {
                let started = (|| {
                    let host = host();
                    let device = match device.as_deref() {
                        Some(wanted) => find_device(host.output_devices()?, wanted)
                            .with_context(|| format!("sortie audio « {wanted} » introuvable"))?,
                        None => host
                            .default_output_device()
                            .context("aucune sortie audio trouvée")?,
                    };
                    let config = device.default_output_config()?;
                    let rate = config.sample_rate();
                    let channels = config.channels();
                    let per_ms = rate as usize * channels as usize / 1000;
                    let buffer = Arc::new(Mutex::new(PlayBuffer {
                        samples: VecDeque::new(),
                        primed: false,
                        prime_len: per_ms * PRIME_MS as usize,
                        max_len: per_ms * MAX_BUFFER_MS as usize,
                        channels: channels as usize,
                    }));
                    let stream = build_output(&device, &config, buffer.clone())?;
                    stream.play()?;
                    Ok((stream, buffer, rate, channels, device_label(&device)))
                })();
                match started {
                    Ok((stream, buffer, rate, channels, label)) => {
                        let _ = ready_tx.send(Ok((buffer, rate, channels, label)));
                        let _ = stop_rx.recv();
                        drop(stream);
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                    }
                }
            })?;
        let (buffer, rate, channels, label) = ready_rx
            .recv()
            .map_err(|_| anyhow!("le thread audio s'est arrêté"))??;
        log::info!("lecture audio : {label}");
        Ok(Self {
            buffer,
            converter: Converter::new(rate, channels),
            _stop: stop_tx,
        })
    }

    pub fn push(&mut self, frame: &AudioFrame) {
        let converted = self.converter.convert(frame);
        if let Ok(mut buffer) = self.buffer.lock() {
            buffer.push(&converted);
        }
    }
}

fn build_output(
    device: &Device,
    config: &SupportedStreamConfig,
    buffer: Arc<Mutex<PlayBuffer>>,
) -> Result<cpal::Stream> {
    with_fallback(config, |stream_config| {
        build_output_format(
            device,
            config.sample_format(),
            stream_config,
            buffer.clone(),
        )
    })
}

fn build_output_format(
    device: &Device,
    format: SampleFormat,
    stream_config: StreamConfig,
    buffer: Arc<Mutex<PlayBuffer>>,
) -> Result<cpal::Stream> {
    match format {
        SampleFormat::F32 => build_output_typed::<f32>(device, stream_config, buffer),
        SampleFormat::F64 => build_output_typed::<f64>(device, stream_config, buffer),
        SampleFormat::I16 => build_output_typed::<i16>(device, stream_config, buffer),
        SampleFormat::I32 => build_output_typed::<i32>(device, stream_config, buffer),
        SampleFormat::U16 => build_output_typed::<u16>(device, stream_config, buffer),
        SampleFormat::U8 => build_output_typed::<u8>(device, stream_config, buffer),
        other => bail!("format audio non géré : {other}"),
    }
}

fn build_output_typed<T>(
    device: &Device,
    config: StreamConfig,
    buffer: Arc<Mutex<PlayBuffer>>,
) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let stream = device.build_output_stream(
        config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| match buffer.lock() {
            Ok(mut buffer) => {
                for out in data.iter_mut() {
                    *out = buffer.pop().to_sample::<T>();
                }
            }
            Err(_) => data.fill(T::EQUILIBRIUM),
        },
        |e| log::warn!("lecture audio : {e}"),
        None,
    )?;
    Ok(stream)
}

/// Convertit le son reçu vers la fréquence et le nombre de canaux de la sortie
/// (interpolation linéaire, état conservé d'un paquet à l'autre).
struct Converter {
    out_rate: u32,
    out_channels: usize,
    in_rate: u32,
    in_channels: usize,
    /// Dernière trame du paquet précédent, pour interpoler à la jonction.
    prev: Vec<f32>,
    /// Position de lecture dans le paquet courant (en trames, -1 = `prev`).
    pos: f64,
}

impl Converter {
    fn new(out_rate: u32, out_channels: u16) -> Self {
        Self {
            out_rate,
            out_channels: out_channels as usize,
            in_rate: 0,
            in_channels: 0,
            prev: Vec::new(),
            pos: 0.0,
        }
    }

    fn convert(&mut self, frame: &AudioFrame) -> Vec<f32> {
        let in_channels = frame.channels as usize;
        if frame.rate != self.in_rate || in_channels != self.in_channels {
            self.in_rate = frame.rate;
            self.in_channels = in_channels;
            self.prev.clear();
            self.pos = 0.0;
        }
        let out_ch = self.out_channels;

        // 1. Canaux : conversion vers le nombre de canaux de la sortie.
        let mapped: Vec<f32> = frame
            .samples
            .chunks_exact(in_channels)
            .flat_map(|input| {
                (0..out_ch).map(move |c| {
                    let to_f32 = |s: i16| s as f32 / 32768.0;
                    if out_ch == 1 && in_channels > 1 {
                        input.iter().map(|&s| to_f32(s)).sum::<f32>() / in_channels as f32
                    } else {
                        to_f32(input[c % in_channels])
                    }
                })
            })
            .collect();

        // 2. Fréquence : interpolation linéaire.
        if frame.rate == self.out_rate {
            return mapped;
        }
        let frames = mapped.len() / out_ch;
        if frames == 0 {
            return Vec::new();
        }
        if self.prev.is_empty() {
            self.prev = mapped[..out_ch].to_vec();
        }
        let step = frame.rate as f64 / self.out_rate as f64;
        let at = |i: isize, c: usize| -> f32 {
            if i < 0 {
                self.prev[c]
            } else {
                mapped[i as usize * out_ch + c]
            }
        };
        let mut out = Vec::with_capacity((frames as f64 / step) as usize * out_ch + out_ch);
        while self.pos < (frames - 1) as f64 {
            let i = self.pos.floor();
            let frac = (self.pos - i) as f32;
            let i = i as isize;
            for c in 0..out_ch {
                let a = at(i, c);
                let b = at(i + 1, c);
                out.push(a + (b - a) * frac);
            }
            self.pos += step;
        }
        self.pos -= frames as f64;
        self.prev = mapped[(frames - 1) * out_ch..].to_vec();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(rate: u32, channels: u16, samples: Vec<i16>) -> AudioFrame {
        AudioFrame {
            rate,
            channels,
            samples,
        }
    }

    #[test]
    fn same_format_is_unchanged() {
        let mut conv = Converter::new(48_000, 2);
        let out = conv.convert(&frame(48_000, 2, vec![16384, -16384, 0, 8192]));
        assert_eq!(out, vec![0.5, -0.5, 0.0, 0.25]);
    }

    #[test]
    fn mono_to_stereo_and_back() {
        let mut conv = Converter::new(48_000, 2);
        assert_eq!(conv.convert(&frame(48_000, 1, vec![16384])), vec![0.5, 0.5]);
        let mut conv = Converter::new(48_000, 1);
        assert_eq!(conv.convert(&frame(48_000, 2, vec![16384, 0])), vec![0.25]);
    }

    #[test]
    fn resampling_keeps_duration() {
        // 1 seconde à 44,1 kHz doit donner ~1 seconde à 48 kHz, paquet par paquet.
        let mut conv = Converter::new(48_000, 1);
        let mut total = 0;
        for _ in 0..100 {
            total += conv.convert(&frame(44_100, 1, vec![1000; 441])).len();
        }
        assert!((47_990..=48_010).contains(&total), "{total}");
    }

    #[test]
    fn buffer_waits_then_drops_excess() {
        let mut buf = PlayBuffer {
            samples: VecDeque::new(),
            primed: false,
            prime_len: 4,
            max_len: 8,
            channels: 2,
        };
        buf.push(&[1.0, 1.0]);
        assert_eq!(buf.pop(), 0.0, "attend d'avoir assez de son");
        buf.push(&[2.0, 2.0]);
        assert_eq!(buf.pop(), 1.0);
        buf.push(&[3.0; 10]);
        assert!(buf.samples.len() <= 8);
        assert_eq!(
            buf.samples.len() % 2,
            1,
            "reste aligné après le premier pop"
        );
    }

    #[test]
    fn invalid_frames_are_rejected() {
        assert!(frame(48_000, 2, vec![0; 4]).is_valid());
        assert!(!frame(48_000, 0, vec![]).is_valid());
        assert!(!frame(1, 2, vec![0; 4]).is_valid());
        assert!(!frame(48_000, 2, vec![0; 3]).is_valid());
    }
}

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

/// Durée des morceaux de son capturés (plus court = moins de latence).
const FRAME_MS: u32 = 5;
/// Tampon de lecture visé au départ : son accumulé avant de démarrer, et niveau que
/// la compensation de dérive cherche à maintenir. Il s'adapte au réseau : agrandi
/// après chaque coupure (à-coups du Wi-Fi), réduit quand tout est stable.
const TARGET_MS: u32 = 80;
const MIN_TARGET_MS: u32 = 60;
const MAX_TARGET_MS: u32 = 300;
const TARGET_STEP_UP_MS: u32 = 40;
const TARGET_STEP_DOWN_MS: u32 = 20;
/// Pause entre deux paquets au-delà de laquelle l'autre PC est considéré
/// silencieux (Windows n'envoie rien quand aucun son ne joue).
const SILENCE_GAP: std::time::Duration = std::time::Duration::from_millis(150);
/// Durée sans coupure avant de réduire le tampon.
const STABLE_BEFORE_SHRINK: std::time::Duration = std::time::Duration::from_secs(20);
/// Marge au-delà de la cible avant de jeter le son en trop (rester synchrone).
const MAX_EXTRA_MS: u32 = 300;
/// Tampon demandé à la carte son (capture et lecture).
const DEVICE_BUFFER_MS: u32 = 20;
/// Correction maximale de la vitesse de lecture (0,2 % : imperceptible). Suffit
/// largement pour la dérive entre deux cartes son (quelques centièmes de %).
const MAX_DRIFT_CORRECTION: f64 = 0.002;
/// Écart au tampon cible (en proportion) toléré sans correction de vitesse.
const DRIFT_DEADBAND: f64 = 0.25;
/// Fondu d'entrée appliqué après une reprise ou un rattrapage.
const FADE_MS: u32 = 5;
/// Au-delà de ce remplissage (en multiple de la cible), le retard est rattrapé
/// d'un coup plutôt que par la correction de vitesse, qui serait trop lente.
const RESYNC_LEVEL: f64 = 2.0;

/// Quand partager le son du système ou le micro.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Jamais.
    Off,
    /// Son : quand l'autre PC contrôle celui-ci. Micro : quand ce PC contrôle l'autre.
    Auto,
    /// Tout le temps, si l'un des deux PC est le PC principal (sinon comme « auto »).
    Always,
}

impl Mode {
    pub const ALL: [Mode; 3] = [Mode::Off, Mode::Auto, Mode::Always];

    pub fn arg(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Auto => "auto",
            Mode::Always => "always",
        }
    }

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
    /// PC principal (souris, clavier, casque) : reçoit le son de l'autre PC.
    pub main: bool,
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
    /// Qualité de transport du son envoyé.
    pub quality: crate::codec::Quality,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Ce qui sort des haut-parleurs du PC.
    System,
    /// Le micro.
    Microphone,
}

/// Paquet de son entrelacé en virgule flottante 32 bits (capture et lecture).
#[derive(Debug, Clone, PartialEq)]
pub struct AudioFrame {
    pub rate: u32,
    pub channels: u16,
    pub samples: Vec<f32>,
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

/// Micro d'un casque Bluetooth (profil mains libres), d'après son nom ou son
/// identifiant : « … Hands-Free AG Audio » sous Windows, `bluez_…` sous Linux.
fn is_bluetooth_mic(device: &Device) -> bool {
    let text = format!("{} {}", device_label(device), id_string(device)).to_lowercase();
    is_bluetooth_name(&text)
}

fn is_bluetooth_name(text: &str) -> bool {
    [
        "hands-free",
        "handsfree",
        "mains libres",
        "bluez",
        "bluetooth",
        "headset_head_unit",
    ]
    .iter()
    .any(|word| text.contains(word))
}

/// Vrai micro utilisable à la place d'un micro Bluetooth : ni Bluetooth, ni
/// son d'une sortie (« monitor »), ni micro virtuel.
fn is_plain_mic(device: &Device) -> bool {
    let text = format!("{} {}", device_label(device), id_string(device)).to_lowercase();
    !is_bluetooth_name(&text)
        && ![
            "monitor",
            "one_mouse",
            "cable output",
            "blackhole",
            "vb-audio",
        ]
        .iter()
        .any(|word| text.contains(word))
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

/// Noms des sorties audio, pour les choix de l'interface.
pub fn output_names() -> Vec<String> {
    names(host().output_devices())
}

/// Noms des entrées audio (micros), hors sources « monitor » (son des sorties).
pub fn input_names() -> Vec<String> {
    names(host().input_devices())
        .into_iter()
        .filter(|name| !name.to_lowercase().starts_with("monitor of"))
        .collect()
}

fn names(devices: Result<impl Iterator<Item = Device>, cpal::Error>) -> Vec<String> {
    let Ok(devices) = devices else {
        return Vec::new();
    };
    let mut names: Vec<String> = devices.map(|d| device_label(&d)).collect();
    names.dedup();
    names
}

/// Sortie à utiliser pour faire du micro reçu un micro utilisable par les applications.
///
/// - Linux : crée (une seule fois) un micro virtuel « Micro one-mouse » avec
///   PulseAudio / PipeWire : une sortie `one_mouse_mic` dont le son ressort par
///   la source `one_mouse_micro`, à choisir comme micro dans Discord, Teams…
/// - Windows / macOS : un micro virtuel demande un pilote ; on utilise VB-Cable
///   ou BlackHole s'ils sont installés.
///
/// `None` : aucun micro virtuel, le micro reçu ne doit pas être joué sur les
/// haut-parleurs (il serait recapturé avec le son et renvoyé : écho).
pub fn virtual_mic_output() -> Option<String> {
    #[cfg(target_os = "linux")]
    if let Some(sink) = linux_virtual_mic() {
        return Some(sink);
    }
    find_virtual_cable(&output_names())
}

/// Câble audio virtuel installé (VB-Cable, BlackHole…) parmi ces sorties.
fn find_virtual_cable(outputs: &[String]) -> Option<String> {
    const KNOWN: [&str; 4] = ["CABLE Input", "VB-Audio", "BlackHole", "Loopback Audio"];
    KNOWN.iter().find_map(|known| {
        outputs
            .iter()
            .find(|name| name.to_lowercase().contains(&known.to_lowercase()))
            .cloned()
    })
}

/// Le pilote de micro virtuel est-il installé ? `None` sous Linux, où one-mouse
/// crée lui-même le micro virtuel.
pub fn virtual_mic_installed(outputs: &[String]) -> Option<bool> {
    if cfg!(target_os = "linux") {
        None
    } else {
        Some(find_virtual_cable(outputs).is_some())
    }
}

#[cfg(target_os = "linux")]
fn linux_virtual_mic() -> Option<String> {
    use std::process::Command;
    const SINK: &str = "one_mouse_mic";
    const SOURCE: &str = "one_mouse_micro";
    let pactl = |args: &[&str]| Command::new("pactl").args(args).output().ok();
    let listed = |kind: &str, name: &str| {
        pactl(&["list", "short", kind]).is_some_and(|out| {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .any(|line| line.split_whitespace().nth(1) == Some(name))
        })
    };
    if !listed("sinks", SINK) {
        let ok = pactl(&[
            "load-module",
            "module-null-sink",
            &format!("sink_name={SINK}"),
            "sink_properties=device.description=one-mouse-micro-interne",
        ])
        .is_some_and(|out| out.status.success());
        if !ok {
            return None;
        }
    }
    if !listed("sources", SOURCE) {
        let ok = pactl(&[
            "load-module",
            "module-remap-source",
            &format!("master={SINK}.monitor"),
            &format!("source_name={SOURCE}"),
            "source_properties=device.description=Micro-one-mouse",
        ])
        .is_some_and(|out| out.status.success());
        if !ok {
            return None;
        }
        log::info!(
            "micro virtuel « Micro-one-mouse » créé : choisissez-le comme micro dans vos applications"
        );
    }
    Some(SINK.to_owned())
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
            let mut device = host.default_input_device().context("aucun micro trouvé")?;
            if is_bluetooth_mic(&device) {
                // Ouvrir le micro d'un casque Bluetooth le bascule en « mode
                // téléphone » : tout son son passe en mono 16 kHz, qualité radio.
                // Un autre micro (intégré au PC, webcam…) garde le casque en stéréo.
                if let Some(other) = host.input_devices()?.find(is_plain_mic) {
                    log::info!(
                        "micro « {} » ignoré (casque Bluetooth : il dégraderait le son en \
                         qualité téléphone), « {} » utilisé à la place",
                        device_label(&device),
                        device_label(&other)
                    );
                    device = other;
                } else {
                    log::warn!(
                        "seul micro disponible : celui du casque Bluetooth, qui passe en \
                         qualité téléphone tant qu'il est utilisé"
                    );
                }
            }
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
pub struct AudioCapture {
    pub frames: mpsc::Receiver<AudioFrame>,
    _stop: std_mpsc::Sender<()>,
}

impl AudioCapture {
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

/// Configuration à faible latence : tampon de ~20 ms au lieu de la valeur par défaut,
/// qui atteint plusieurs secondes avec PulseAudio.
fn low_latency(config: &SupportedStreamConfig) -> StreamConfig {
    let mut stream_config = config.config();
    let wanted = stream_config.sample_rate * DEVICE_BUFFER_MS / 1000;
    stream_config.buffer_size = match *config.buffer_size() {
        SupportedBufferSize::Range { min, max } => BufferSize::Fixed(wanted.clamp(min, max)),
        SupportedBufferSize::Unknown => BufferSize::Fixed(wanted),
    };
    stream_config
}

/// Sous Linux, essaie d'abord un petit tampon (PulseAudio en prend sinon plusieurs
/// secondes), puis la configuration par défaut. Ailleurs (WASAPI, CoreAudio), le
/// réglage par défaut est déjà court : en imposer un plus petit provoque des
/// craquements.
fn with_fallback(
    config: &SupportedStreamConfig,
    mut build: impl FnMut(StreamConfig) -> Result<cpal::Stream>,
) -> Result<cpal::Stream> {
    if cfg!(target_os = "linux") {
        if let Ok(stream) = build(low_latency(config)) {
            return Ok(stream);
        }
        log::debug!("tampon audio court refusé, configuration par défaut");
    }
    build(config.config())
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
    f32: FromSample<T>,
{
    let rate = config.sample_rate;
    let channels = config.channels;
    let frame_len = (rate * FRAME_MS / 1000) as usize * channels as usize;
    let mut buffer = Vec::with_capacity(frame_len);
    let stream = device.build_input_stream(
        config,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            for &sample in data {
                buffer.push(sample.to_sample::<f32>());
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
    /// Niveau visé (échantillons) : accumulé avant de (re)démarrer la lecture.
    prime_len: usize,
    max_len: usize,
    channels: usize,
    /// Nombre de fois où le tampon s'est vidé (coupure audible).
    underruns: u64,
    /// Longueur du fondu d'entrée (échantillons) après une reprise ou un rattrapage.
    fade_len: usize,
    /// Échantillons de fondu restant à appliquer.
    fade_left: usize,
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
            // Reprise en fondu : pas de « clic » quand le son repart.
            self.fade_left = self.fade_len;
        }
        match self.samples.pop_front() {
            Some(s) if self.fade_left > 0 => {
                let gain = 1.0 - self.fade_left as f32 / self.fade_len as f32;
                self.fade_left -= 1;
                s * gain
            }
            Some(s) => s,
            None => {
                self.primed = false;
                self.underruns += 1;
                0.0
            }
        }
    }
}

/// Lecture du son reçu de l'autre PC.
pub struct AudioPlayer {
    buffer: Arc<Mutex<PlayBuffer>>,
    converter: Converter,
    /// Remplissage du tampon lissé, en proportion de la cible (1.0 = à la cible).
    fill: f64,
    /// Échantillons par milliseconde de la sortie.
    per_ms: usize,
    /// Tampon visé actuellement (adaptatif).
    target_ms: u32,
    /// Coupures déjà prises en compte.
    seen_underruns: u64,
    /// Dernier changement de taille du tampon.
    last_change: std::time::Instant,
    /// Arrivée du paquet précédent.
    last_push: std::time::Instant,
    _stop: std_mpsc::Sender<()>,
}

impl AudioPlayer {
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
                        prime_len: per_ms * TARGET_MS as usize,
                        max_len: per_ms * (TARGET_MS + MAX_EXTRA_MS) as usize,
                        channels: channels as usize,
                        underruns: 0,
                        fade_len: per_ms * FADE_MS as usize,
                        fade_left: 0,
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
            fill: 1.0,
            per_ms: rate as usize * channels as usize / 1000,
            target_ms: TARGET_MS,
            seen_underruns: 0,
            last_change: std::time::Instant::now(),
            last_push: std::time::Instant::now(),
            _stop: stop_tx,
        })
    }

    pub fn push(&mut self, frame: &AudioFrame) {
        let shared = self.buffer.clone();
        let Ok(mut buffer) = shared.lock() else {
            return;
        };
        let gap = self.last_push.elapsed();
        self.last_push = std::time::Instant::now();
        self.adapt_target(&mut buffer, gap);
        // Compensation de dérive : tampon trop rempli → lecture un peu plus rapide,
        // trop vide → un peu plus lente. Lissé pour ne pas moduler la hauteur du son.
        if buffer.primed {
            let level = buffer.samples.len() as f64 / buffer.prime_len as f64;
            if level > RESYNC_LEVEL {
                // Gros retard (rafale réseau, démarrage) : retour direct à la cible.
                let excess = buffer.samples.len() - buffer.prime_len;
                let excess = excess - excess % buffer.channels;
                buffer.samples.drain(..excess);
                buffer.fade_left = buffer.fade_len;
                self.fill = 1.0;
            } else {
                // Lissage sur ~2 s : les à-coups du réseau ne modulent pas la vitesse.
                self.fill = 0.995 * self.fill + 0.005 * level;
            }
        }
        let speed = drift_speed(self.fill);
        let converted = self.converter.convert(frame, speed);
        buffer.push(&converted);
    }
}

impl AudioPlayer {
    /// Tampon adaptatif : plus grand après une coupure, plus petit après 20 s sans.
    fn adapt_target(&mut self, buffer: &mut PlayBuffer, gap: std::time::Duration) {
        if buffer.underruns > self.seen_underruns && gap > SILENCE_GAP {
            // Le son s'est arrêté chez l'autre PC (silence : rien à envoyer), ce
            // n'est pas le réseau : inutile d'agrandir le tampon.
            self.seen_underruns = buffer.underruns;
            return;
        }
        let target = if buffer.underruns > self.seen_underruns {
            let missed = buffer.underruns - self.seen_underruns;
            self.seen_underruns = buffer.underruns;
            let target = (self.target_ms + TARGET_STEP_UP_MS).min(MAX_TARGET_MS);
            log::info!(
                "son : {missed} coupure(s), tampon porté à {target} ms (réseau irrégulier ?)"
            );
            target
        } else if self.last_change.elapsed() > STABLE_BEFORE_SHRINK
            && self.target_ms > MIN_TARGET_MS
        {
            (self.target_ms - TARGET_STEP_DOWN_MS).max(MIN_TARGET_MS)
        } else {
            return;
        };
        self.last_change = std::time::Instant::now();
        if target != self.target_ms {
            self.target_ms = target;
            buffer.prime_len = self.per_ms * target as usize;
            buffer.max_len = self.per_ms * (target + MAX_EXTRA_MS) as usize;
        }
    }
}

/// Vitesse de lecture selon le remplissage lissé du tampon (1.0 = à la cible).
/// Aucune correction tant que le tampon reste à ±25 % de sa cible : la hauteur
/// du son ne varie jamais pour rien.
fn drift_speed(fill: f64) -> f64 {
    let error = fill - 1.0;
    if error.abs() < DRIFT_DEADBAND {
        return 1.0;
    }
    let error = error - DRIFT_DEADBAND.copysign(error);
    1.0 + (error * MAX_DRIFT_CORRECTION).clamp(-MAX_DRIFT_CORRECTION, MAX_DRIFT_CORRECTION)
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

/// Convertit le son reçu vers la fréquence et le nombre de canaux de la sortie.
///
/// Coefficient des voies mélangées à -3 dB (centre, surround).
const MINUS_3DB: f32 = std::f32::consts::FRAC_1_SQRT_2;

/// Gauche et droite d'une trame, selon les dispositions standard de Windows,
/// macOS et PulseAudio : 3 = FL FR FC, 4 = FL FR BL BR, 5 = FL FR FC BL BR,
/// 6 = FL FR FC LFE BL BR, 7 = FL FR FC LFE BC SL SR, 8 = FL FR FC LFE BL BR SL SR.
/// Le centre va des deux côtés à -3 dB, chaque surround de son côté à -3 dB, le
/// caisson de basses est ignoré (comme le fait Windows en stéréo).
fn stereo_of(frame: &[f32]) -> (f32, f32) {
    let g = MINUS_3DB;
    let [l, r] = [frame[0], frame[1]];
    let (center, left, right) = match frame.len() {
        3 => (frame[2], 0.0, 0.0),
        4 => (0.0, frame[2], frame[3]),
        5 => (frame[2], frame[3], frame[4]),
        6 => (frame[2], frame[4], frame[5]),
        7 => (frame[2], frame[5] + g * frame[4], frame[6] + g * frame[4]),
        n if n >= 8 => (frame[2], frame[4] + frame[6], frame[5] + frame[7]),
        _ => (0.0, 0.0, 0.0),
    };
    (
        soft_limit(l + g * (center + left)),
        soft_limit(r + g * (center + right)),
    )
}

/// Laisse passer le son intact jusqu'à 0,9 puis l'arrondit en douceur vers 1 :
/// un mixage multicanal fort ne sature jamais en craquant.
fn soft_limit(x: f32) -> f32 {
    const KNEE: f32 = 0.9;
    let a = x.abs();
    if a <= KNEE {
        x
    } else {
        x.signum() * (KNEE + (1.0 - KNEE) * ((a - KNEE) / (1.0 - KNEE)).tanh())
    }
}

/// Convertit des échantillons entrelacés de `in_ch` vers `out_ch` canaux.
///
/// Mono et stéréo vont uniquement sur l'avant gauche et l'avant droite de la
/// sortie : les autres voies restent muettes (sinon un casque 7.1 entendrait
/// gauche et droite mélangées partout, son métallique qui saute d'un côté à
/// l'autre). Le multicanal est d'abord ramené en stéréo par un vrai mixage, qui
/// garde le centre (les voix).
pub(crate) fn remix(samples: &[f32], in_ch: usize, out_ch: usize) -> Vec<f32> {
    if in_ch == 0 || out_ch == 0 {
        return Vec::new();
    }
    if in_ch == out_ch {
        let whole = samples.len() / in_ch * in_ch;
        return samples[..whole].to_vec();
    }
    let frames = samples.len() / in_ch;
    let mut out = Vec::with_capacity(frames * out_ch);
    for frame in samples.chunks_exact(in_ch) {
        let (l, r) = match in_ch {
            1 => (frame[0], frame[0]),
            2 => (frame[0], frame[1]),
            _ => stereo_of(frame),
        };
        if out_ch == 1 {
            out.push(0.5 * (l + r));
        } else {
            out.push(l);
            out.push(r);
            out.extend(std::iter::repeat_n(0.0, out_ch - 2));
        }
    }
    out
}

/// Interpolation par sinc fenêtré (Blackman, 32 points) avec filtre
/// anti-repliement quand la fréquence baisse : qualité des logiciels audio
/// professionnels. État conservé d'un paquet à l'autre. `speed` accélère ou
/// ralentit très légèrement la lecture pour compenser la dérive entre l'horloge
/// de la carte son qui capture et celle qui joue.
pub(crate) struct Converter {
    out_rate: u32,
    out_channels: usize,
    in_rate: u32,
    in_channels: usize,
    /// Dernières trames du paquet précédent (entrelacées).
    history: Vec<f32>,
    /// Position de lecture, en trames, dans `history` + paquet courant.
    pos: f64,
    /// Coefficients du filtre pour la position courante (réutilisés).
    weights: Vec<f64>,
}

/// Demi-largeur du filtre sinc, en trames d'entrée.
const SINC_HALF: usize = 16;
/// Trames gardées d'un paquet à l'autre pour le filtre.
const HISTORY: usize = 2 * SINC_HALF;

impl Converter {
    pub(crate) fn new(out_rate: u32, out_channels: u16) -> Self {
        Self {
            out_rate,
            out_channels: out_channels as usize,
            in_rate: 0,
            in_channels: 0,
            history: Vec::new(),
            pos: HISTORY as f64,
            weights: vec![0.0; 2 * SINC_HALF],
        }
    }

    pub(crate) fn convert(&mut self, frame: &AudioFrame, speed: f64) -> Vec<f32> {
        let in_channels = frame.channels as usize;
        let out_ch = self.out_channels;
        if frame.rate != self.in_rate || in_channels != self.in_channels {
            self.in_rate = frame.rate;
            self.in_channels = in_channels;
            self.history.clear();
            self.pos = HISTORY as f64;
        }

        // 1. Canaux : mixage vers le nombre de canaux de la sortie.
        let mapped = remix(&frame.samples, in_channels, out_ch);
        let frames = mapped.len() / out_ch;
        if frames == 0 {
            return Vec::new();
        }
        if self.history.is_empty() {
            self.history = mapped[..out_ch].repeat(HISTORY);
        }

        // 2. Même fréquence et pas de correction : copie directe.
        if frame.rate == self.out_rate && speed == 1.0 {
            self.history.extend_from_slice(&mapped);
            let excess = self.history.len() - HISTORY * out_ch;
            self.history.drain(..excess);
            self.pos = HISTORY as f64;
            return mapped;
        }

        // 3. Rééchantillonnage par sinc fenêtré sur l'historique suivi du paquet.
        let mut src = Vec::with_capacity(self.history.len() + mapped.len());
        src.extend_from_slice(&self.history);
        src.extend_from_slice(&mapped);
        let total = src.len() / out_ch;
        let step = frame.rate as f64 / self.out_rate as f64 * speed;
        // Fréquence de coupure (relative à la moitié de la fréquence d'entrée) :
        // abaissée quand on réduit la fréquence, pour ne pas créer de repliement.
        let cutoff = (1.0 / step).min(1.0);
        let mut out = Vec::with_capacity((frames as f64 / step) as usize * out_ch + out_ch);
        while (self.pos.floor() as usize) + SINC_HALF < total {
            let i = self.pos.floor() as usize;
            let first = i + 1 - SINC_HALF;
            let mut sum = 0.0;
            for (tap, weight) in self.weights.iter_mut().enumerate() {
                let x = self.pos - (first + tap) as f64;
                *weight = sinc_kernel(x, cutoff);
                sum += *weight;
            }
            for c in 0..out_ch {
                let mut acc = 0.0;
                for (tap, weight) in self.weights.iter().enumerate() {
                    acc += src[(first + tap) * out_ch + c] as f64 * weight;
                }
                // Normalisation : gain exactement 1 pour un signal continu.
                out.push((acc / sum) as f32);
            }
            self.pos += step;
        }
        self.pos -= (total - HISTORY) as f64;
        self.history = src[(total - HISTORY) * out_ch..].to_vec();
        out
    }
}

/// Noyau sinc fenêtré (Blackman) de demi-largeur `SINC_HALF`.
fn sinc_kernel(x: f64, cutoff: f64) -> f64 {
    use std::f64::consts::PI;
    let half = SINC_HALF as f64;
    if x.abs() >= half {
        return 0.0;
    }
    let sinc = if x.abs() < 1e-9 {
        1.0
    } else {
        (PI * cutoff * x).sin() / (PI * cutoff * x)
    };
    let n = (x + half) / (2.0 * half);
    let window = 0.42 - 0.5 * (2.0 * PI * n).cos() + 0.08 * (4.0 * PI * n).cos();
    cutoff * sinc * window
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Paquet de test à partir d'échantillons 16 bits (plus lisibles).
    fn frame(rate: u32, channels: u16, samples: Vec<i16>) -> AudioFrame {
        AudioFrame {
            rate,
            channels,
            samples: samples.into_iter().map(|s| s as f32 / 32768.0).collect(),
        }
    }

    #[test]
    fn remix_stereo_to_surround_uses_front_only() {
        let out = remix(&[0.25, -0.5, 0.1, 0.2], 2, 8);
        assert_eq!(out.len(), 16);
        assert_eq!(&out[..2], &[0.25, -0.5]);
        assert!(out[2..8].iter().all(|&s| s == 0.0));
        assert_eq!(&out[8..10], &[0.1, 0.2]);
        assert!(out[10..].iter().all(|&s| s == 0.0));
        let out = remix(&[0.25, -0.5], 2, 6);
        assert_eq!(out, vec![0.25, -0.5, 0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn remix_surround_to_stereo_keeps_center() {
        // Voix au centre seulement (7.1) : elle doit sortir des deux côtés.
        let mut frame = [0.0f32; 8];
        frame[2] = 0.5;
        let out = remix(&frame, 8, 2);
        assert!((out[0] - 0.5 * MINUS_3DB).abs() < 1e-6);
        assert_eq!(out[0], out[1]);
        // Surround gauche : seulement à gauche ; caisson ignoré.
        let out = remix(&[0.0, 0.0, 0.0, 0.9, 0.3, 0.0], 6, 2);
        assert!((out[0] - 0.3 * MINUS_3DB).abs() < 1e-6);
        assert_eq!(out[1], 0.0);
    }

    #[test]
    fn remix_never_clips() {
        let out = remix(&[1.0; 8], 8, 2);
        assert!(out.iter().all(|&s| s > 0.9 && s <= 1.0));
        // En dessous du seuil, rien ne change.
        assert_eq!(soft_limit(0.5), 0.5);
    }

    #[test]
    fn bluetooth_mics_detected() {
        assert!(is_bluetooth_name("casque (wh-1000xm4 hands-free ag audio)"));
        assert!(is_bluetooth_name("bluez_input.aa_bb_cc.headset-head-unit"));
        assert!(!is_bluetooth_name("microphone (realtek(r) audio)"));
    }

    #[test]
    fn remix_mono() {
        assert_eq!(remix(&[0.5, 0.1], 1, 2), vec![0.5, 0.5, 0.1, 0.1]);
        assert_eq!(remix(&[0.2, 0.4], 2, 1), vec![0.3]);
        let out = remix(&[0.0, 0.0, 0.4, 0.0, 0.0, 0.0], 6, 1);
        assert!((out[0] - 0.4 * MINUS_3DB).abs() < 1e-6);
    }

    #[test]
    fn same_format_is_unchanged() {
        let mut conv = Converter::new(48_000, 2);
        let out = conv.convert(&frame(48_000, 2, vec![16384, -16384, 0, 8192]), 1.0);
        assert_eq!(out, vec![0.5, -0.5, 0.0, 0.25]);
    }

    #[test]
    fn mono_to_stereo_and_back() {
        let mut conv = Converter::new(48_000, 2);
        assert_eq!(
            conv.convert(&frame(48_000, 1, vec![16384]), 1.0),
            vec![0.5, 0.5]
        );
        let mut conv = Converter::new(48_000, 1);
        assert_eq!(
            conv.convert(&frame(48_000, 2, vec![16384, 0]), 1.0),
            vec![0.25]
        );
    }

    #[test]
    fn resampling_keeps_duration() {
        // 1 seconde à 44,1 kHz doit donner ~1 seconde à 48 kHz, paquet par paquet.
        let mut conv = Converter::new(48_000, 1);
        let mut total = 0;
        for _ in 0..100 {
            total += conv.convert(&frame(44_100, 1, vec![1000; 441]), 1.0).len();
        }
        // Le filtre garde SINC_HALF trames en réserve (0,3 ms) pour regarder « en avant ».
        assert!((47_970..=48_010).contains(&total), "{total}");
    }

    /// Sinus de 1 kHz à 44,1 kHz converti en 48 kHz par paquets de 10 ms :
    /// l'erreur par rapport au sinus idéal reste très faible.
    #[test]
    fn resampling_is_accurate() {
        let freq = 1000.0f64;
        let mut conv = Converter::new(48_000, 1);
        let mut out = Vec::new();
        let mut n = 0usize;
        for _ in 0..50 {
            let samples = (0..441)
                .map(|_| {
                    let v = (2.0 * std::f64::consts::PI * freq * n as f64 / 44_100.0).sin();
                    n += 1;
                    (v * 16384.0) as i16
                })
                .collect();
            out.extend(conv.convert(&frame(44_100, 1, samples), 1.0));
        }
        let mut max_err = 0.0f64;
        for (k, &v) in out.iter().enumerate().skip(100).take(40_000) {
            let t = k as f64 / 48_000.0;
            let ideal = 0.5 * (2.0 * std::f64::consts::PI * freq * t).sin();
            max_err = max_err.max((v as f64 - ideal).abs());
        }
        // Limité par la quantification 16 bits du signal de test (±0,00003).
        assert!(max_err < 0.002, "erreur max {max_err}");
    }

    /// Sens inverse : 48 kHz vers 44,1 kHz (son reçu d'Opus vers une sortie à 44,1 kHz).
    #[test]
    fn downsampling_is_accurate() {
        let freq = 1000.0f64;
        let mut conv = Converter::new(44_100, 1);
        let mut out = Vec::new();
        let mut n = 0usize;
        for _ in 0..50 {
            let samples = (0..480)
                .map(|_| {
                    let v = (2.0 * std::f64::consts::PI * freq * n as f64 / 48_000.0).sin();
                    n += 1;
                    (v * 16384.0) as i16
                })
                .collect();
            out.extend(conv.convert(&frame(48_000, 1, samples), 1.0));
        }
        let mut max_err = 0.0f64;
        for (k, &v) in out.iter().enumerate().skip(100).take(20_000) {
            let t = k as f64 / 44_100.0;
            let ideal = 0.5 * (2.0 * std::f64::consts::PI * freq * t).sin();
            max_err = max_err.max((v as f64 - ideal).abs());
        }
        assert!(max_err < 0.002, "erreur max {max_err}");
    }

    #[test]
    fn drift_correction_is_bounded() {
        assert_eq!(drift_speed(1.0), 1.0);
        assert_eq!(drift_speed(1.2), 1.0, "zone neutre");
        assert_eq!(drift_speed(0.8), 1.0, "zone neutre");
        assert!(drift_speed(1.5) > 1.0 && drift_speed(1.5) <= 1.0 + MAX_DRIFT_CORRECTION);
        assert!(drift_speed(0.5) < 1.0 && drift_speed(0.5) >= 1.0 - MAX_DRIFT_CORRECTION);
        assert_eq!(drift_speed(100.0), 1.0 + MAX_DRIFT_CORRECTION);
    }

    #[test]
    fn faster_speed_produces_fewer_samples() {
        let mut normal = Converter::new(48_000, 1);
        let mut fast = Converter::new(48_000, 1);
        let (mut a, mut b) = (0, 0);
        for _ in 0..100 {
            a += normal
                .convert(&frame(48_000, 1, vec![1000; 480]), 1.0)
                .len();
            b += fast
                .convert(&frame(48_000, 1, vec![1000; 480]), 1.002)
                .len();
        }
        assert_eq!(a, 48_000);
        assert!((47_880..=47_920).contains(&b), "{b}");
    }

    #[test]
    fn playback_restarts_with_a_fade() {
        let mut buf = PlayBuffer {
            samples: VecDeque::new(),
            primed: false,
            prime_len: 4,
            max_len: 100,
            channels: 1,
            underruns: 0,
            fade_len: 4,
            fade_left: 0,
        };
        buf.push(&[1.0; 8]);
        let out: Vec<f32> = (0..6).map(|_| buf.pop()).collect();
        assert_eq!(out, vec![0.0, 0.25, 0.5, 0.75, 1.0, 1.0]);
    }

    #[test]
    fn buffer_waits_then_drops_excess() {
        let mut buf = PlayBuffer {
            samples: VecDeque::new(),
            primed: false,
            prime_len: 4,
            max_len: 8,
            channels: 2,
            underruns: 0,
            fade_len: 0,
            fade_left: 0,
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
}

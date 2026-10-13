//! Mises à jour automatiques depuis les releases GitHub.
//!
//! La fenêtre vérifie la dernière release au démarrage puis régulièrement. Une
//! version plus récente est téléchargée, vérifiée (empreinte SHA-256 publiée par
//! GitHub) puis installée :
//! - Windows : installateur lancé en mode silencieux, one-mouse relancé ensuite ;
//! - Linux (AppImage) : le fichier est remplacé puis relancé ;
//! - Linux (.deb) et macOS : la page de téléchargement est ouverte.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use sha2::{Digest, Sha256};

const LATEST: &str = "https://api.github.com/repos/TheDEMON78/one-mouse/releases/latest";

/// Version de ce programme (injectée par la CI à chaque release).
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[derive(Debug, Clone, Deserialize)]
struct ApiRelease {
    tag_name: String,
    html_url: String,
    assets: Vec<ApiAsset>,
}

#[derive(Debug, Clone, Deserialize)]
struct ApiAsset {
    name: String,
    browser_download_url: String,
    /// « sha256:… »
    digest: Option<String>,
}

/// Version plus récente disponible.
#[derive(Debug, Clone)]
pub struct Release {
    pub version: String,
    /// Page de la release (téléchargement manuel).
    pub page: String,
    asset: Option<ApiAsset>,
}

impl Release {
    /// La mise à jour peut s'installer seule sur ce système.
    pub fn automatic(&self) -> bool {
        self.asset.is_some()
    }
}

/// Mise à jour téléchargée et vérifiée, prête à être installée.
pub struct Prepared {
    file: PathBuf,
}

fn agent() -> ureq::Agent {
    agent_with_redirects(10)
}

fn agent_with_redirects(redirects: u32) -> ureq::Agent {
    let tls = ureq::tls::TlsConfig::builder()
        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
        .build();
    ureq::Agent::new_with_config(
        ureq::config::Config::builder()
            .tls_config(tls)
            .timeout_global(Some(Duration::from_secs(120)))
            .max_redirects(redirects)
            .http_status_as_error(redirects > 0)
            .user_agent(format!("one-mouse/{}", current_version()))
            .build(),
    )
}

/// Adresse finale d'un fichier de release : GitHub redirige vers son stockage.
/// La redirection est suivie à la main, sans lire le corps de la réponse 302
/// (mal formé derrière certains proxys).
fn resolve_download(url: &str) -> Result<String> {
    let response = agent_with_redirects(0)
        .get(url)
        .call()
        .context("téléchargement de la mise à jour impossible")?;
    let status = response.status();
    if status.is_redirection() {
        let location = response
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .context("redirection sans adresse")?;
        return Ok(location.to_owned());
    }
    ensure!(status.is_success(), "téléchargement refusé ({status})");
    Ok(url.to_owned())
}

fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    let mut parts = text.trim_start_matches('v').split('.');
    let version = (
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    );
    parts.next().is_none().then_some(version)
}

/// Fichier de la release qui s'installe seul ici, s'il y en a un.
fn pick_asset(assets: &[ApiAsset]) -> Option<ApiAsset> {
    let wanted = |name: &str| -> bool {
        if cfg!(windows) {
            name.ends_with("_x64-setup.exe")
        } else if cfg!(target_os = "linux") {
            // Seule l'AppImage se remplace sans droits d'administrateur.
            std::env::var_os("APPIMAGE").is_some() && name.ends_with("_x86_64.AppImage")
        } else {
            false
        }
    };
    assets.iter().find(|a| wanted(&a.name)).cloned()
}

/// Dernière release, si elle est plus récente que ce programme.
pub fn check() -> Result<Option<Release>> {
    let release: ApiRelease = agent()
        .get(LATEST)
        .header("Accept", "application/vnd.github+json")
        .call()
        .context("vérification des mises à jour impossible")?
        .body_mut()
        .read_json()
        .context("réponse de GitHub illisible")?;
    let latest = parse_version(&release.tag_name).context("numéro de version inconnu")?;
    let current = parse_version(current_version()).context("version actuelle inconnue")?;
    if latest <= current {
        return Ok(None);
    }
    Ok(Some(Release {
        version: release.tag_name.trim_start_matches('v').to_owned(),
        page: release.html_url,
        asset: pick_asset(&release.assets),
    }))
}

/// Télécharge la mise à jour et vérifie son empreinte (peut prendre du temps).
pub fn prepare(release: &Release) -> Result<Prepared> {
    let asset = release
        .asset
        .as_ref()
        .context("pas de mise à jour automatique sur ce système")?;
    let url = resolve_download(&asset.browser_download_url)?;
    let data = agent()
        .get(&url)
        .call()
        .context("téléchargement de la mise à jour impossible")?
        .body_mut()
        .with_config()
        .limit(200 * 1024 * 1024)
        .read_to_vec()
        .context("téléchargement de la mise à jour interrompu")?;
    if let Some(expected) = asset
        .digest
        .as_deref()
        .and_then(|d| d.strip_prefix("sha256:"))
    {
        let actual = hex(&Sha256::digest(&data));
        ensure!(
            actual.eq_ignore_ascii_case(expected),
            "mise à jour corrompue (empreinte différente) : non installée"
        );
    }
    let file = std::env::temp_dir().join(&asset.name);
    std::fs::write(&file, &data).context("enregistrement de la mise à jour impossible")?;
    Ok(Prepared { file })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Lance l'installation. one-mouse doit se fermer juste après (le moteur arrêté) :
/// l'installateur remplace le programme puis le relance.
pub fn launch(prepared: Prepared) -> Result<()> {
    let exe = std::env::current_exe().context("chemin de l'application introuvable")?;
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        // Attendre que one-mouse soit fermé, installer en silence, puis relancer.
        let script = format!(
            "timeout /t 2 /nobreak >nul & \"{}\" /S & start \"\" \"{}\"",
            prepared.file.display(),
            exe.display()
        );
        std::process::Command::new("cmd")
            .arg("/c")
            .raw_arg(script)
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .context("lancement de l'installateur impossible")?;
        Ok(())
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = exe;
        let target = PathBuf::from(std::env::var_os("APPIMAGE").context("pas une AppImage")?);
        std::fs::set_permissions(&prepared.file, std::fs::Permissions::from_mode(0o755))?;
        // Copie à côté puis renommage : le remplacement est atomique.
        let staged = target.with_extension("AppImage.new");
        std::fs::copy(&prepared.file, &staged)?;
        std::fs::rename(&staged, &target).context("remplacement de l'AppImage impossible")?;
        std::process::Command::new(&target)
            .spawn()
            .context("relance de one-mouse impossible")?;
        Ok(())
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = (exe, prepared);
        bail!("pas de mise à jour automatique sur ce système")
    }
}

/// Le moteur et l'interface doivent avoir la même version : refuse d'installer
/// une version plus ancienne (protection en cas de release retirée).
#[cfg_attr(not(test), allow(dead_code))]
fn is_newer(candidate: &str) -> Result<bool> {
    let candidate = parse_version(candidate).context("numéro de version inconnu")?;
    let current = parse_version(current_version()).context("version actuelle inconnue")?;
    if candidate < current {
        bail!("version plus ancienne");
    }
    Ok(candidate > current)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically() {
        assert_eq!(parse_version("v1.0.29"), Some((1, 0, 29)));
        assert_eq!(parse_version("1.1.1"), Some((1, 1, 1)));
        assert_eq!(parse_version("1.0"), None);
        assert_eq!(parse_version("1.0.x"), None);
        // 1.0.100 n'existe pas (1.1.0), mais 1.0.9 < 1.0.10 numériquement.
        assert!(parse_version("1.0.10") > parse_version("1.0.9"));
        assert!(parse_version("1.1.0") > parse_version("1.0.99"));
        assert!(is_newer("999.0.0").unwrap());
    }

    /// Interroge vraiment GitHub (réseau nécessaire) : `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn live_check_reaches_github() {
        let release = check().unwrap();
        println!("{release:?}");
        // Avec APPIMAGE défini (Linux), télécharge aussi et vérifie l'empreinte.
        if let Some(release) = release.filter(Release::automatic) {
            let prepared = prepare(&release).unwrap();
            println!("téléchargé et vérifié : {}", prepared.file.display());
        }
    }

    #[test]
    fn hex_digest() {
        assert_eq!(hex(&[0x00, 0xab, 0x10]), "00ab10");
    }
}

/// Où en est la mise à jour, pour la fenêtre.
#[derive(Debug, Clone)]
pub enum UpdateState {
    Checking,
    UpToDate,
    /// Plus récente, à télécharger (ou à récupérer à la main si `automatic()` est faux).
    Available(Release),
    Downloading(String),
    /// Téléchargée et vérifiée : prête à être installée.
    Ready(String),
    Failed(String),
}

/// Vérifie régulièrement les mises à jour dans un thread et télécharge celles qui
/// s'installent seules quand la mise à jour automatique est activée.
pub struct Updater {
    state: std::sync::Arc<std::sync::Mutex<UpdateState>>,
    prepared: std::sync::Arc<std::sync::Mutex<Option<Prepared>>>,
    auto: std::sync::Arc<std::sync::atomic::AtomicBool>,
    wake: std::sync::mpsc::Sender<Command>,
}

enum Command {
    Check,
    Download,
}

/// Délai entre deux vérifications.
const CHECK_EVERY: Duration = Duration::from_secs(6 * 3600);

impl Updater {
    pub fn start(auto: bool, repaint: impl Fn() + Send + 'static) -> Self {
        use std::sync::atomic::Ordering;
        use std::sync::{Arc, Mutex};
        let state = Arc::new(Mutex::new(UpdateState::Checking));
        let prepared: Arc<Mutex<Option<Prepared>>> = Arc::default();
        let auto = Arc::new(std::sync::atomic::AtomicBool::new(auto));
        let (wake, commands) = std::sync::mpsc::channel();
        let (st, prep, auto_flag) = (state.clone(), prepared.clone(), auto.clone());
        let set = move |s: UpdateState| {
            *st.lock().expect("verrou") = s;
            repaint();
        };
        let _ = std::thread::Builder::new()
            .name("mises-a-jour".into())
            .spawn(move || {
                let mut command = Command::Check;
                loop {
                    let mut release = None;
                    if matches!(command, Command::Check) {
                        set(UpdateState::Checking);
                        match check() {
                            Ok(Some(found)) => {
                                log::info!("nouvelle version disponible : {}", found.version);
                                set(UpdateState::Available(found.clone()));
                                release = Some(found);
                            }
                            Ok(None) => set(UpdateState::UpToDate),
                            Err(e) => {
                                log::debug!("mises à jour : {e:#}");
                                set(UpdateState::Failed(format!("{e:#}")));
                            }
                        }
                    }
                    if let Command::Download = command
                        && let Ok(Some(found)) = check()
                    {
                        release = Some(found);
                    }
                    let download =
                        matches!(command, Command::Download) || auto_flag.load(Ordering::Relaxed);
                    if let Some(found) = release.filter(|r| r.automatic() && download) {
                        set(UpdateState::Downloading(found.version.clone()));
                        match prepare(&found) {
                            Ok(ready) => {
                                *prep.lock().expect("verrou") = Some(ready);
                                set(UpdateState::Ready(found.version));
                            }
                            Err(e) => set(UpdateState::Failed(format!("{e:#}"))),
                        }
                    }
                    command = match commands.recv_timeout(CHECK_EVERY) {
                        Ok(next) => next,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Command::Check,
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    };
                }
            });
        Self {
            state,
            prepared,
            auto,
            wake,
        }
    }

    pub fn state(&self) -> UpdateState {
        self.state.lock().expect("verrou").clone()
    }

    pub fn check_now(&self) {
        let _ = self.wake.send(Command::Check);
    }

    /// Télécharge la version disponible (mise à jour automatique désactivée).
    pub fn download_now(&self) {
        let _ = self.wake.send(Command::Download);
    }

    pub fn set_auto(&self, auto: bool) {
        self.auto.store(auto, std::sync::atomic::Ordering::Relaxed);
    }

    /// Mise à jour prête, retirée pour être installée.
    pub fn take_ready(&self) -> Option<Prepared> {
        self.prepared.lock().expect("verrou").take()
    }
}

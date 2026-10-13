//! Le micro reçu de l'autre PC devient le micro par défaut de celui-ci pendant
//! la connexion : toutes les applications (Discord, Teams…) utilisent alors le
//! micro du PC qui contrôle, sans réglage. L'ancien micro est rétabli ensuite.

/// Rétablit l'ancien micro par défaut quand il est détruit.
pub struct DefaultMicGuard {
    #[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
    previous: String,
}

/// Met par défaut le micro virtuel associé à la sortie `output` où est joué le
/// micro reçu. `None` si ce n'est pas possible (le micro reste alors à choisir
/// dans les applications).
pub fn switch_to_virtual(output: &str) -> Option<DefaultMicGuard> {
    let guard = imp::switch(output);
    match &guard {
        Some(_) => log::info!("le micro de l'autre PC est maintenant le micro par défaut de ce PC"),
        None => log::info!(
            "micro par défaut inchangé : choisissez le micro virtuel dans vos applications"
        ),
    }
    guard
}

impl Drop for DefaultMicGuard {
    fn drop(&mut self) {
        imp::restore(&self.previous);
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::process::Command;

    use super::DefaultMicGuard;

    /// Source créée par `audio::virtual_mic_output` pour la sortie `one_mouse_mic`.
    const SOURCE: &str = "one_mouse_micro";

    fn pactl(args: &[&str]) -> Option<String> {
        let out = Command::new("pactl").args(args).output().ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }

    pub fn switch(output: &str) -> Option<DefaultMicGuard> {
        if output != "one_mouse_mic" {
            return None;
        }
        let previous = pactl(&["get-default-source"])?;
        pactl(&["set-default-source", SOURCE])?;
        Some(DefaultMicGuard { previous })
    }

    pub fn restore(previous: &str) {
        if !previous.is_empty() && previous != SOURCE {
            let _ = pactl(&["set-default-source", previous]);
        }
    }
}

#[cfg(windows)]
mod imp {
    use std::process::Stdio;

    use super::DefaultMicGuard;

    const SCRIPT: &str = include_str!("windows.ps1");

    /// Lance le script ; renvoie le micro par défaut d'avant si tout s'est bien passé.
    fn run(var: &str, value: &str) -> Option<String> {
        let out = crate::powershell::command(SCRIPT)
            .env(var, value)
            .stderr(Stdio::null())
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }

    pub fn switch(output: &str) -> Option<DefaultMicGuard> {
        // La sortie « CABLE Input » ressort par le micro « CABLE Output » ;
        // les autres câbles virtuels (BlackHole…) portent le même nom des deux côtés.
        let target = if output.to_lowercase().contains("cable input") {
            "CABLE Output".to_owned()
        } else {
            output.to_owned()
        };
        let previous = run("ONE_MOUSE_MIC_TARGET", &target)?;
        Some(DefaultMicGuard { previous })
    }

    pub fn restore(previous: &str) {
        if !previous.is_empty() {
            let _ = run("ONE_MOUSE_MIC_RESTORE", previous);
        }
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod imp {
    use super::DefaultMicGuard;

    pub fn switch(_output: &str) -> Option<DefaultMicGuard> {
        None
    }

    pub fn restore(_previous: &str) {}
}

//! Sortie son par défaut : jamais un câble audio virtuel.
//!
//! Le micro reçu de l'autre PC est joué dans un câble virtuel (VB-Cable,
//! « Micro-one-mouse »…). Si ce câble est aussi la sortie par défaut du PC —
//! VB-Cable se met souvent par défaut à son installation —, le son capturé pour
//! l'autre PC contient ce micro : on s'entend en écho. one-mouse remet alors une
//! vraie sortie (haut-parleurs, casque) par défaut.

/// Si la sortie par défaut est un câble virtuel, met une vraie sortie par défaut
/// et renvoie son nom. `None` : rien à faire, ou impossible.
pub fn leave_virtual_output() -> Option<String> {
    imp::leave_virtual_output()
}

/// Nom de sortie qui désigne un câble audio virtuel.
pub fn is_virtual_output(name: &str) -> bool {
    let name = name.to_lowercase();
    [
        "cable input",
        "vb-audio",
        "blackhole",
        "one_mouse",
        "one-mouse",
        "virtual audio",
    ]
    .iter()
    .any(|word| name.contains(word))
}

#[cfg(windows)]
mod imp {
    use std::process::Stdio;

    const SCRIPT: &str = include_str!("windows.ps1");

    pub fn leave_virtual_output() -> Option<String> {
        let out = crate::powershell::command(SCRIPT)
            .stderr(Stdio::null())
            .output()
            .ok()?;
        (out.status.code() == Some(0))
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::process::Command;

    use super::is_virtual_output;

    fn pactl(args: &[&str]) -> Option<String> {
        let out = Command::new("pactl").args(args).output().ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }

    pub fn leave_virtual_output() -> Option<String> {
        let current = pactl(&["get-default-sink"])?;
        if !is_virtual_output(&current) {
            return None;
        }
        // `pactl list short sinks` : numéro, nom, pilote, format, état.
        let sinks = pactl(&["list", "short", "sinks"])?;
        let real = sinks
            .lines()
            .filter_map(|line| line.split('\t').nth(1))
            .find(|name| !is_virtual_output(name))?
            .to_owned();
        pactl(&["set-default-sink", &real])?;
        Some(real)
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod imp {
    pub fn leave_virtual_output() -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::is_virtual_output;

    #[test]
    fn virtual_outputs_detected() {
        for name in [
            "CABLE Input (VB-Audio Virtual Cable)",
            "one_mouse_mic",
            "BlackHole 2ch",
        ] {
            assert!(is_virtual_output(name), "{name}");
        }
        for name in [
            "Haut-parleurs (Realtek(R) Audio)",
            "Casque (WH-1000XM4)",
            "alsa_output.pci-0000_00_1f.3.analog-stereo",
        ] {
            assert!(!is_virtual_output(name), "{name}");
        }
    }
}

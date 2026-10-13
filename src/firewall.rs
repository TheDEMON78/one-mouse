//! Pare-feu Windows : autorise les connexions entrantes vers one-mouse.
//!
//! Sans règle, Windows bloque en silence les connexions de l'autre PC (erreur
//! 10060 côté connexion). Si la fenêtre « Autoriser l'accès » a été refusée ou
//! n'est pas apparue, Windows crée même des règles de blocage à son nom, qui
//! l'emportent sur toute autorisation : on les supprime avant d'ajouter la nôtre.

/// `Some(true)` si la règle one-mouse existe, `None` si ce système n'est pas concerné.
pub fn is_allowed() -> Option<bool> {
    imp::is_allowed()
}

/// Ajoute la règle (demande de confirmation administrateur de Windows).
pub fn allow() -> anyhow::Result<()> {
    imp::allow()
}

#[cfg(windows)]
mod imp {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};

    use anyhow::{Context, Result, bail};

    const RULE: &str = "one-mouse";
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    pub fn is_allowed() -> Option<bool> {
        let output = Command::new("netsh")
            .args([
                "advfirewall",
                "firewall",
                "show",
                "rule",
                &format!("name={RULE}"),
                "dir=in",
            ])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .ok()?;
        // Une règle de blocage au même nom ne compte pas comme une autorisation.
        let text = String::from_utf8_lossy(&output.stdout).to_lowercase();
        Some(output.status.success() && !text.contains("block") && !text.contains("bloquer"))
    }

    pub fn allow() -> Result<()> {
        let exe = std::env::current_exe().context("chemin de l'application introuvable")?;
        let exe = exe.display().to_string();
        let port = crate::DEFAULT_PORT;
        // Toutes les commandes dans une seule élévation : une seule confirmation.
        let commands = format!(
            "netsh advfirewall firewall delete rule name=\"{RULE}\" & \
             netsh advfirewall firewall delete rule name=\"{RULE}.exe\" & \
             netsh advfirewall firewall add rule name=\"{RULE}\" dir=in action=allow \
             program=\"{exe}\" enable=yes profile=any protocol=TCP localport={port}"
        );
        // Chaîne PowerShell entre apostrophes : on double celles du chemin.
        let script = format!(
            "$p = Start-Process -FilePath cmd -ArgumentList '/c {}' -Verb RunAs -WindowStyle Hidden -Wait -PassThru; exit $p.ExitCode",
            commands.replace('\'', "''")
        );
        let status = crate::powershell::command(&script)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context("lancement de PowerShell impossible")?;
        if is_allowed() != Some(true) {
            bail!(
                "la règle n'a pas été ajoutée (confirmation refusée ?, code {})",
                status.code().unwrap_or(-1)
            );
        }
        Ok(())
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn is_allowed() -> Option<bool> {
        None
    }

    pub fn allow() -> anyhow::Result<()> {
        Ok(())
    }
}

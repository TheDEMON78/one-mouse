//! Lancement de one-mouse à l'ouverture de session.

use anyhow::{Context, Result};

/// Indique si one-mouse est lancé automatiquement à l'ouverture de session.
pub fn is_enabled() -> bool {
    imp::is_enabled()
}

pub fn set_enabled(enabled: bool) -> Result<()> {
    let exe = std::env::current_exe().context("chemin de l'application introuvable")?;
    imp::set_enabled(enabled, exe)
}

#[cfg(windows)]
mod imp {
    use std::os::windows::process::CommandExt;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};

    use anyhow::{Result, bail};

    const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    const VALUE: &str = "one-mouse";
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    fn reg(args: &[&str]) -> bool {
        Command::new("reg")
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .status()
            .is_ok_and(|s| s.success())
    }

    pub fn is_enabled() -> bool {
        reg(&["query", RUN_KEY, "/v", VALUE])
    }

    pub fn set_enabled(enabled: bool, exe: PathBuf) -> Result<()> {
        let ok = if enabled {
            let command = format!("\"{}\"", exe.display());
            reg(&[
                "add", RUN_KEY, "/v", VALUE, "/t", "REG_SZ", "/d", &command, "/f",
            ])
        } else {
            !is_enabled() || reg(&["delete", RUN_KEY, "/v", VALUE, "/f"])
        };
        if !ok {
            bail!("modification du registre impossible");
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::path::PathBuf;

    use anyhow::Result;

    fn path() -> Option<PathBuf> {
        directories::BaseDirs::new().map(|d| {
            d.home_dir()
                .join("Library/LaunchAgents/com.bloopstudio.onemouse.plist")
        })
    }

    pub fn is_enabled() -> bool {
        path().is_some_and(|p| p.exists())
    }

    pub fn set_enabled(enabled: bool, exe: PathBuf) -> Result<()> {
        let path = path().ok_or_else(|| anyhow::anyhow!("dossier personnel introuvable"))?;
        if !enabled {
            let _ = std::fs::remove_file(&path);
            return Ok(());
        }
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>com.bloopstudio.onemouse</string>
  <key>ProgramArguments</key><array><string>{}</string></array>
  <key>RunAtLoad</key><true/>
</dict>
</plist>
"#,
            super::xml_escape(&exe.display().to_string())
        );
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, plist)?;
        Ok(())
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod imp {
    use std::path::PathBuf;

    use anyhow::Result;

    fn path() -> Option<PathBuf> {
        directories::BaseDirs::new().map(|d| d.config_dir().join("autostart/one-mouse.desktop"))
    }

    pub fn is_enabled() -> bool {
        path().is_some_and(|p| p.exists())
    }

    pub fn set_enabled(enabled: bool, exe: PathBuf) -> Result<()> {
        let path = path().ok_or_else(|| anyhow::anyhow!("dossier personnel introuvable"))?;
        if !enabled {
            let _ = std::fs::remove_file(&path);
            return Ok(());
        }
        // Une AppImage se déplace : on lance le fichier AppImage lui-même.
        let exe = std::env::var_os("APPIMAGE")
            .map(PathBuf::from)
            .unwrap_or(exe);
        let entry = format!(
            "[Desktop Entry]\nType=Application\nName=one-mouse\nExec=\"{}\"\nIcon=one-mouse\nX-GNOME-Autostart-enabled=true\n",
            exe.display()
        );
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, entry)?;
        Ok(())
    }
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

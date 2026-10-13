//! Les deux PC vivent ensemble : verrouillage commun et pas de mise en veille.
//!
//! - Verrouiller un PC (Windows+L, menu…) verrouille aussi l'autre. Le
//!   déverrouillage reste propre à chaque PC (mot de passe).
//! - Tant que les deux PC sont connectés, aucun ne se met en veille : la
//!   connexion, le son et le micro ne sont pas coupés. L'écran peut s'éteindre.
//!
//! Windows : WTS (état de la session), LockWorkStation, SetThreadExecutionState.
//! Linux : logind par D-Bus (LockedHint, Lock, Inhibit « sleep »).
//! macOS : anti-veille par `caffeinate` seulement.

/// Accès à l'alimentation et au verrouillage de la session, pour une connexion.
pub struct Power {
    inner: imp::Power,
}

impl Power {
    /// `keep_awake` : empêche la mise en veille tant que l'objet existe.
    pub async fn new(keep_awake: bool) -> Self {
        Self {
            inner: imp::Power::new(keep_awake).await,
        }
    }

    /// La session de ce PC est-elle verrouillée ?
    pub async fn is_locked(&self) -> bool {
        self.inner.is_locked().await
    }

    /// Verrouille la session de ce PC.
    pub async fn lock(&self) {
        self.inner.lock().await;
    }
}

#[cfg(windows)]
mod imp {
    use std::ptr::null_mut;

    use windows_sys::Win32::System::Power::{
        ES_CONTINUOUS, ES_SYSTEM_REQUIRED, SetThreadExecutionState,
    };
    use windows_sys::Win32::System::RemoteDesktop::{
        WTS_CURRENT_SERVER_HANDLE, WTS_CURRENT_SESSION, WTS_SESSIONSTATE_LOCK, WTSFreeMemory,
        WTSINFOEXW, WTSQuerySessionInformationW, WTSSessionInfoEx,
    };
    use windows_sys::Win32::System::Shutdown::LockWorkStation;

    pub struct Power {
        awake: bool,
    }

    impl Power {
        pub async fn new(keep_awake: bool) -> Self {
            if keep_awake {
                // SAFETY : simple indicateur pour ce thread, retiré à la destruction.
                unsafe {
                    SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED);
                }
            }
            Self { awake: keep_awake }
        }

        pub async fn is_locked(&self) -> bool {
            let mut buffer = null_mut();
            let mut size = 0u32;
            // SAFETY : tampon alloué par WTS, lu selon sa taille puis libéré.
            unsafe {
                if WTSQuerySessionInformationW(
                    WTS_CURRENT_SERVER_HANDLE,
                    WTS_CURRENT_SESSION,
                    WTSSessionInfoEx,
                    &mut buffer,
                    &mut size,
                ) == 0
                    || buffer.is_null()
                {
                    return false;
                }
                let locked = size as usize >= size_of::<WTSINFOEXW>() && {
                    let info = &*(buffer as *const WTSINFOEXW);
                    info.Level == 1
                        && info.Data.WTSInfoExLevel1.SessionFlags == WTS_SESSIONSTATE_LOCK as i32
                };
                WTSFreeMemory(buffer.cast());
                locked
            }
        }

        pub async fn lock(&self) {
            // SAFETY : sans argument.
            unsafe {
                LockWorkStation();
            }
        }
    }

    impl Drop for Power {
        fn drop(&mut self) {
            if self.awake {
                // SAFETY : retour au comportement normal pour ce thread.
                unsafe {
                    SetThreadExecutionState(ES_CONTINUOUS);
                }
            }
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use zbus::zvariant::{OwnedFd, OwnedObjectPath};

    pub struct Power {
        conn: Option<zbus::Connection>,
        session: Option<OwnedObjectPath>,
        /// Tant que ce descripteur est ouvert, logind n'endort pas le PC.
        _inhibit: Option<OwnedFd>,
    }

    async fn manager(conn: &zbus::Connection) -> zbus::Result<zbus::Proxy<'static>> {
        zbus::Proxy::new(
            conn,
            "org.freedesktop.login1",
            "/org/freedesktop/login1",
            "org.freedesktop.login1.Manager",
        )
        .await
    }

    impl Power {
        pub async fn new(keep_awake: bool) -> Self {
            let conn = zbus::Connection::system().await.ok();
            let mut session = None;
            let mut inhibit = None;
            if let Some(conn) = &conn
                && let Ok(manager) = manager(conn).await
            {
                session = manager
                    .call::<_, _, OwnedObjectPath>("GetSessionByPID", &(std::process::id(),))
                    .await
                    .ok();
                if keep_awake {
                    inhibit = manager
                        .call::<_, _, OwnedFd>(
                            "Inhibit",
                            &(
                                "sleep",
                                "one-mouse",
                                "souris, clavier et son partagés avec l'autre PC",
                                "block",
                            ),
                        )
                        .await
                        .map_err(|e| log::debug!("anti-veille indisponible : {e}"))
                        .ok();
                }
            }
            Self {
                conn,
                session,
                _inhibit: inhibit,
            }
        }

        async fn session(&self) -> Option<zbus::Proxy<'_>> {
            zbus::Proxy::new(
                self.conn.as_ref()?,
                "org.freedesktop.login1",
                self.session.as_ref()?.as_str(),
                "org.freedesktop.login1.Session",
            )
            .await
            .ok()
        }

        pub async fn is_locked(&self) -> bool {
            match self.session().await {
                Some(session) => session
                    .get_property::<bool>("LockedHint")
                    .await
                    .unwrap_or(false),
                None => false,
            }
        }

        pub async fn lock(&self) {
            if let Some(session) = self.session().await
                && let Err(e) = session.call::<_, _, ()>("Lock", &()).await
            {
                log::warn!("verrouillage impossible : {e}");
            }
        }
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod imp {
    pub struct Power {
        caffeinate: Option<std::process::Child>,
    }

    impl Power {
        pub async fn new(keep_awake: bool) -> Self {
            // macOS : `caffeinate -i` empêche la veille tant que one-mouse tourne.
            let caffeinate = keep_awake
                .then(|| {
                    std::process::Command::new("caffeinate")
                        .args(["-i", "-w", &std::process::id().to_string()])
                        .spawn()
                        .ok()
                })
                .flatten();
            Self { caffeinate }
        }

        pub async fn is_locked(&self) -> bool {
            false
        }

        pub async fn lock(&self) {}
    }

    impl Drop for Power {
        fn drop(&mut self) {
            if let Some(child) = &mut self.caffeinate {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

//! Bluetooth direct sous Linux : profil RFCOMM enregistré auprès de BlueZ.
//!
//! BlueZ publie le service, gère l'association et transmet chaque connexion
//! établie à notre objet D-Bus `org.bluez.Profile1` sous la forme d'un socket.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::sync::mpsc;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

use super::{NO_PAIRED, NOT_FOUND, SERVICE_UUID, Stream, bridge};

const PROFILE_PATH: &str = "/com/bloopstudio/onemouse/bluetooth";
/// Canal RFCOMM du service (annoncé par SDP : l'autre PC le découvre seul).
const CHANNEL: u16 = 22;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Objet D-Bus auquel BlueZ remet les connexions établies.
struct Profile {
    connections: mpsc::Sender<(OwnedFd, String)>,
}

#[zbus::interface(name = "org.bluez.Profile1")]
impl Profile {
    fn release(&self) {}

    async fn new_connection(
        &self,
        device: ObjectPath<'_>,
        fd: zbus::zvariant::OwnedFd,
        _properties: HashMap<String, OwnedValue>,
    ) {
        let _ = self.connections.send((fd.into(), device.to_string())).await;
    }

    fn request_disconnection(&self, _device: ObjectPath<'_>) {}
}

/// Profil enregistré : désenregistré par BlueZ quand la connexion D-Bus se ferme.
struct Registered {
    conn: zbus::Connection,
    connections: mpsc::Receiver<(OwnedFd, String)>,
}

async fn register(role: &str) -> Result<Registered> {
    let conn = zbus::Connection::system().await.map_err(|e| {
        log::debug!("bus système D-Bus : {e}");
        anyhow::anyhow!(
            "service Bluetooth du système inaccessible (Bluetooth installé et démarré ?)"
        )
    })?;
    let (tx, rx) = mpsc::channel(4);
    conn.object_server()
        .at(PROFILE_PATH, Profile { connections: tx })
        .await?;
    let mut options: HashMap<&str, Value> = HashMap::new();
    options.insert("Name", Value::from("one-mouse"));
    options.insert("Role", Value::from(role));
    options.insert("RequireAuthentication", Value::from(true));
    options.insert("RequireAuthorization", Value::from(false));
    if role == "server" {
        options.insert("Channel", Value::from(CHANNEL));
    }
    let manager = zbus::Proxy::new(
        &conn,
        "org.bluez",
        "/org/bluez",
        "org.bluez.ProfileManager1",
    )
    .await?;
    manager
        .call::<_, _, ()>(
            "RegisterProfile",
            &(ObjectPath::try_from(PROFILE_PATH)?, SERVICE_UUID, options),
        )
        .await
        .context("service Bluetooth (BlueZ) indisponible : Bluetooth activé ?")?;
    Ok(Registered {
        conn,
        connections: rx,
    })
}

/// Socket reçu de BlueZ, prêt pour les threads de lecture et d'écriture.
fn into_stream(fd: OwnedFd, keep_alive: Box<dyn Send>) -> Result<Stream> {
    let socket = UnixStream::from(fd);
    socket.set_nonblocking(false)?;
    Ok(bridge(socket, keep_alive))
}

fn as_bool(value: Option<&OwnedValue>) -> Option<bool> {
    value.and_then(|v| bool::try_from(&**v).ok())
}

/// Objets BlueZ : (chemin, interfaces → propriétés).
async fn managed_objects(
    conn: &zbus::Connection,
) -> Result<
    Vec<(
        OwnedObjectPath,
        HashMap<String, HashMap<String, OwnedValue>>,
    )>,
> {
    let manager = zbus::fdo::ObjectManagerProxy::builder(conn)
        .destination("org.bluez")?
        .path("/")?
        .build()
        .await?;
    Ok(manager
        .get_managed_objects()
        .await?
        .into_iter()
        .map(|(path, ifaces)| {
            let ifaces = ifaces
                .into_iter()
                .map(|(name, props)| (name.to_string(), props))
                .collect();
            (path, ifaces)
        })
        .collect())
}

/// PC associés (chemin D-Bus, nom), les plus probables d'abord.
async fn paired_computers(conn: &zbus::Connection) -> Result<Vec<(OwnedObjectPath, String)>> {
    let mut devices = Vec::new();
    for (path, ifaces) in managed_objects(conn).await? {
        let Some(device) = ifaces.get("org.bluez.Device1") else {
            continue;
        };
        if as_bool(device.get("Paired")) != Some(true) {
            continue;
        }
        let class = device.get("Class").and_then(|v| u32::try_from(&**v).ok());
        // Classe « ordinateur » (ou inconnue) : pas les casques, souris, téléphones.
        if class.is_some_and(|c| (c >> 8) & 0x1f != 1) {
            continue;
        }
        let name = device
            .get("Alias")
            .and_then(|v| String::try_from(v.try_clone().ok()?).ok())
            .unwrap_or_else(|| path.to_string());
        let connected = as_bool(device.get("Connected")) == Some(true);
        devices.push((connected, path, name));
    }
    // Les PC déjà reliés d'abord.
    devices.sort_by_key(|(connected, _, _)| !connected);
    Ok(devices.into_iter().map(|(_, p, n)| (p, n)).collect())
}

async fn alias(conn: &zbus::Connection, device: &str) -> Option<String> {
    let proxy = zbus::Proxy::new(conn, "org.bluez", device, "org.bluez.Device1")
        .await
        .ok()?;
    proxy.get_property::<String>("Alias").await.ok()
}

/// Rend ce PC visible et associable pendant l'attente (3 minutes, durée par
/// défaut de BlueZ, renouvelée à chaque attente).
async fn set_discoverable(conn: &zbus::Connection) {
    let Ok(objects) = managed_objects(conn).await else {
        return;
    };
    for (path, ifaces) in objects {
        if !ifaces.contains_key("org.bluez.Adapter1") {
            continue;
        }
        if let Ok(adapter) =
            zbus::Proxy::new(conn, "org.bluez", path.as_str(), "org.bluez.Adapter1").await
        {
            let _ = adapter.set_property("Pairable", true).await;
            let _ = adapter.set_property("Discoverable", true).await;
        }
    }
}

pub struct Server {
    registered: Registered,
}

impl Server {
    pub async fn start() -> Result<Self> {
        let registered = register("server").await?;
        set_discoverable(&registered.conn).await;
        log::info!("Bluetooth : service one-mouse publié, ce PC est visible pour l'association");
        Ok(Self { registered })
    }

    pub async fn accept(&mut self) -> Result<(Stream, String)> {
        let (fd, device) = self
            .registered
            .connections
            .recv()
            .await
            .context("service Bluetooth arrêté")?;
        let name = alias(&self.registered.conn, &device)
            .await
            .unwrap_or(device);
        // Visible à nouveau pour une prochaine association.
        set_discoverable(&self.registered.conn).await;
        Ok((into_stream(fd, Box::new(()))?, name))
    }
}

pub async fn connect_paired() -> Result<(Stream, String)> {
    let mut registered = register("client").await?;
    let devices = paired_computers(&registered.conn).await?;
    if devices.is_empty() {
        bail!(NO_PAIRED);
    }
    let mut tried = Vec::new();
    for (path, name) in devices {
        let device = zbus::Proxy::new(
            &registered.conn,
            "org.bluez",
            path.as_str(),
            "org.bluez.Device1",
        )
        .await?;
        let attempt = tokio::time::timeout(
            CONNECT_TIMEOUT,
            device.call::<_, _, ()>("ConnectProfile", &(SERVICE_UUID,)),
        )
        .await;
        match attempt {
            Ok(Ok(())) => {
                let received =
                    tokio::time::timeout(Duration::from_secs(5), registered.connections.recv())
                        .await;
                if let Ok(Some((fd, _))) = received {
                    // La connexion D-Bus reste ouverte tant que dure la session.
                    let conn = registered.conn.clone();
                    return Ok((into_stream(fd, Box::new(conn))?, name));
                }
            }
            Ok(Err(e)) => {
                log::debug!("Bluetooth : {name} : {e}");
                tried.push(format!("{name} : {e}"));
            }
            Err(_) => tried.push(format!("{name} : ne répond pas")),
        }
    }
    bail!("{NOT_FOUND} ({})", tried.join(" ; "))
}

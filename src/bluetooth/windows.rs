//! Bluetooth direct sous Windows : sockets RFCOMM de Winsock.
//!
//! Le service one-mouse est publié par SDP (`WSASetServiceW`) : le PC qui se
//! connecte le trouve sur l'autre PC à partir de son seul identifiant, sans
//! numéro de canal à connaître.

use std::io;
use std::mem::{size_of, zeroed};
use std::ptr::{from_mut, null_mut};
use std::sync::Once;

use anyhow::{Context, Result, bail};
use windows_sys::Win32::Devices::Bluetooth::{
    AF_BTH, BLUETOOTH_DEVICE_INFO, BLUETOOTH_DEVICE_SEARCH_PARAMS, BTHPROTO_RFCOMM,
    BluetoothEnableDiscovery, BluetoothEnableIncomingConnections, BluetoothFindDeviceClose,
    BluetoothFindFirstDevice, BluetoothFindNextDevice, NS_BTH, SOCKADDR_BTH,
};
use windows_sys::Win32::Networking::WinSock::{
    CSADDR_INFO, INVALID_SOCKET, RNRSERVICE_DELETE, RNRSERVICE_REGISTER, SD_BOTH, SOCK_STREAM,
    SOCKADDR, SOCKET, SOCKET_ADDRESS, WSADATA, WSAQUERYSETW, WSASetServiceW, WSAStartup, accept,
    bind, closesocket, connect, getsockname, listen, recv, send, shutdown, socket,
};
use windows_sys::core::GUID;

use super::{BlockingSocket, NO_PAIRED, NOT_FOUND, Stream, bridge};

/// [`super::SERVICE_UUID`] au format de Windows.
const SERVICE_GUID: GUID = GUID {
    data1: 0x6f6e_652d,
    data2: 0x6d6f,
    data3: 0x7573,
    data4: [0x65, 0x2d, 0x62, 0x6c, 0x6f, 0x6f, 0x70, 0x31],
};
/// Canal RFCOMM choisi par Windows.
const BT_PORT_ANY: u32 = u32::MAX;

fn winsock_startup() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY : WSADATA est rempli par l'appel.
        unsafe {
            let mut data: WSADATA = zeroed();
            WSAStartup(0x0202, &mut data);
        }
    });
}

struct Socket(SOCKET);

impl Socket {
    fn new() -> io::Result<Self> {
        winsock_startup();
        // SAFETY : appel Winsock simple, résultat vérifié.
        let raw = unsafe { socket(AF_BTH as i32, SOCK_STREAM, BTHPROTO_RFCOMM as i32) };
        if raw == INVALID_SOCKET {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(raw))
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        // SAFETY : socket ouvert par nous, fermé une seule fois.
        unsafe {
            closesocket(self.0);
        }
    }
}

impl BlockingSocket for Socket {
    fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        let len = buf.len().min(i32::MAX as usize) as i32;
        // SAFETY : `buf` est valide pour `len` octets.
        let n = unsafe { recv(self.0, buf.as_mut_ptr(), len, 0) };
        if n < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(n as usize)
        }
    }

    fn send(&self, buf: &[u8]) -> io::Result<usize> {
        let len = buf.len().min(i32::MAX as usize) as i32;
        // SAFETY : `buf` est valide pour `len` octets.
        let n = unsafe { send(self.0, buf.as_ptr(), len, 0) };
        if n < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(n as usize)
        }
    }

    fn shutdown(&self) {
        // SAFETY : socket valide tant que `self` existe.
        unsafe {
            shutdown(self.0, SD_BOTH);
        }
    }
}

fn sockaddr(btaddr: u64, service: GUID, port: u32) -> SOCKADDR_BTH {
    SOCKADDR_BTH {
        addressFamily: AF_BTH,
        btAddr: btaddr,
        serviceClassId: service,
        port,
    }
}

/// PC associés (adresse, nom). Classe « ordinateur » ou inconnue seulement.
fn paired_computers() -> Vec<(u64, String)> {
    let params = BLUETOOTH_DEVICE_SEARCH_PARAMS {
        dwSize: size_of::<BLUETOOTH_DEVICE_SEARCH_PARAMS>() as u32,
        fReturnAuthenticated: 1,
        fReturnRemembered: 1,
        fReturnUnknown: 0,
        fReturnConnected: 1,
        fIssueInquiry: 0,
        cTimeoutMultiplier: 0,
        hRadio: null_mut(),
    };
    let mut info = BLUETOOTH_DEVICE_INFO {
        dwSize: size_of::<BLUETOOTH_DEVICE_INFO>() as u32,
        ..Default::default()
    };
    let mut devices = Vec::new();
    // SAFETY : structures initialisées avec leur taille, recherche refermée.
    unsafe {
        let find = BluetoothFindFirstDevice(&params, &mut info);
        if find.is_null() {
            return devices;
        }
        loop {
            let class = info.ulClassofDevice;
            let computer = class == 0 || (class >> 8) & 0x1f == 1;
            if info.fAuthenticated != 0 && computer {
                let len = info.szName.iter().position(|&c| c == 0).unwrap_or(0);
                let name = String::from_utf16_lossy(&info.szName[..len]);
                devices.push((info.Address.Anonymous.ullLong, name));
            }
            info.dwSize = size_of::<BLUETOOTH_DEVICE_INFO>() as u32;
            if BluetoothFindNextDevice(find, &mut info) == 0 {
                break;
            }
        }
        BluetoothFindDeviceClose(find);
    }
    devices
}

fn device_name(address: u64) -> String {
    paired_computers()
        .into_iter()
        .find(|(a, _)| *a == address)
        .map(|(_, name)| name)
        .unwrap_or_else(|| format!("{address:012X}"))
}

/// Enregistrement SDP du service : données gardées en vie pour le retirer.
struct Registration {
    name: Vec<u16>,
    guid: GUID,
    addr: SOCKADDR_BTH,
    csa: CSADDR_INFO,
}

impl Registration {
    fn set(&mut self, operation: i32) -> io::Result<()> {
        self.csa = CSADDR_INFO {
            LocalAddr: SOCKET_ADDRESS {
                lpSockaddr: from_mut(&mut self.addr).cast::<SOCKADDR>(),
                iSockaddrLength: size_of::<SOCKADDR_BTH>() as i32,
            },
            RemoteAddr: SOCKET_ADDRESS {
                lpSockaddr: null_mut(),
                iSockaddrLength: 0,
            },
            iSocketType: SOCK_STREAM,
            iProtocol: BTHPROTO_RFCOMM as i32,
        };
        // SAFETY : tous les pointeurs visent des champs de `self`, vivants pendant l'appel.
        unsafe {
            let mut query: WSAQUERYSETW = zeroed();
            query.dwSize = size_of::<WSAQUERYSETW>() as u32;
            query.lpszServiceInstanceName = self.name.as_mut_ptr();
            query.lpServiceClassId = &mut self.guid;
            query.dwNameSpace = NS_BTH;
            query.dwNumberOfCsAddrs = 1;
            query.lpcsaBuffer = &mut self.csa;
            if WSASetServiceW(&query, operation, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }
}

pub struct Server {
    socket: Socket,
    registration: Box<Registration>,
}

impl Server {
    pub async fn start() -> Result<Self> {
        let socket = Socket::new().context("Bluetooth indisponible (activé sur ce PC ?)")?;
        let mut addr = sockaddr(0, GUID::from_u128(0), BT_PORT_ANY);
        let mut len = size_of::<SOCKADDR_BTH>() as i32;
        // SAFETY : adresse valide pour sa taille ; résultats vérifiés.
        unsafe {
            let raw = from_mut(&mut addr).cast::<SOCKADDR>();
            if bind(socket.0, raw, len) != 0 {
                return Err(io::Error::last_os_error()).context("service Bluetooth impossible");
            }
            if listen(socket.0, 1) != 0 {
                return Err(io::Error::last_os_error()).context("service Bluetooth impossible");
            }
            getsockname(socket.0, raw, &mut len);
        }
        let mut registration = Box::new(Registration {
            name: "one-mouse".encode_utf16().chain([0]).collect(),
            guid: SERVICE_GUID,
            addr,
            // SAFETY : rempli par `set` avant tout usage.
            csa: unsafe { zeroed() },
        });
        registration
            .set(RNRSERVICE_REGISTER)
            .context("publication du service Bluetooth impossible")?;
        // Visible et joignable pour l'association, pendant que one-mouse attend.
        // SAFETY : `null` = toutes les radios Bluetooth.
        unsafe {
            BluetoothEnableIncomingConnections(null_mut(), 1);
            BluetoothEnableDiscovery(null_mut(), 1);
        }
        log::info!("Bluetooth : service one-mouse publié, ce PC est visible pour l'association");
        Ok(Self {
            socket,
            registration,
        })
    }

    pub async fn accept(&mut self) -> Result<(Stream, String)> {
        let raw = self.socket.0;
        let (client, address) = tokio::task::spawn_blocking(move || {
            let mut addr = sockaddr(0, GUID::from_u128(0), 0);
            let mut len = size_of::<SOCKADDR_BTH>() as i32;
            // SAFETY : `raw` reste ouvert tant que `Server` existe ; s'il est fermé,
            // `accept` échoue proprement.
            let client = unsafe { accept(raw, from_mut(&mut addr).cast::<SOCKADDR>(), &mut len) };
            if client == INVALID_SOCKET {
                return Err(io::Error::last_os_error());
            }
            let address = addr.btAddr;
            Ok((Socket(client), address))
        })
        .await?
        .context("connexion Bluetooth entrante impossible")?;
        let name = tokio::task::spawn_blocking(move || device_name(address)).await?;
        Ok((bridge(client, Box::new(())), name))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.registration.set(RNRSERVICE_DELETE);
    }
}

pub async fn connect_paired() -> Result<(Stream, String)> {
    let (socket, name) = tokio::task::spawn_blocking(|| {
        let devices = paired_computers();
        if devices.is_empty() {
            bail!(NO_PAIRED);
        }
        for (address, name) in devices {
            let socket = Socket::new().context("Bluetooth indisponible (activé sur ce PC ?)")?;
            let mut target = sockaddr(address, SERVICE_GUID, 0);
            // SAFETY : adresse valide pour sa taille. Windows trouve le canal par SDP.
            let ok = unsafe {
                connect(
                    socket.0,
                    from_mut(&mut target).cast::<SOCKADDR>(),
                    size_of::<SOCKADDR_BTH>() as i32,
                )
            } == 0;
            if ok {
                return Ok((socket, name));
            }
            log::debug!("Bluetooth : {name} : {}", io::Error::last_os_error());
        }
        bail!(NOT_FOUND)
    })
    .await??;
    Ok((bridge(socket, Box::new(())), name))
}

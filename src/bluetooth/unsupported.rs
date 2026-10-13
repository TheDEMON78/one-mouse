//! Bluetooth direct pas encore disponible sur ce système.

use anyhow::{Result, bail};

use super::{KnownPeer, Stream};

const UNSUPPORTED: &str = "Bluetooth direct pas encore disponible sur ce système : utilisez le \
     réseau, ou un réseau personnel Bluetooth avec l'adresse de l'autre PC";

pub struct Server;

impl Server {
    pub async fn start() -> Result<Self> {
        bail!(UNSUPPORTED)
    }

    pub async fn accept(&mut self) -> Result<(Stream, String)> {
        bail!(UNSUPPORTED)
    }
}

pub async fn connect_paired(_known: Option<KnownPeer>) -> Result<(Stream, String)> {
    bail!(UNSUPPORTED)
}

pub async fn local_address() -> Option<u64> {
    None
}

pub mod native;
pub mod wifidirect;

use crate::config::Band;
use anyhow::Result;

pub enum Backend {
    Native(native::NativeHotspot),
    WifiDirect(wifidirect::WifiDirectAp),
}

impl Backend {
    pub fn stop(&mut self) {
        match self {
            Backend::Native(h) => {
                let _ = h.stop();
            }
            Backend::WifiDirect(h) => h.stop(),
        }
    }
}

pub fn band_of(band: Band) -> windows::Networking::NetworkOperators::TetheringWiFiBand {
    use windows::Networking::NetworkOperators::TetheringWiFiBand;
    match band {
        Band::Auto => TetheringWiFiBand::Auto,
        Band::TwoPointFour => TetheringWiFiBand::TwoPointFourGigahertz,
        Band::Five => TetheringWiFiBand::FiveGigahertz,
    }
}

pub fn start_native(ssid: &str, pass: &str, band: Band) -> Result<native::NativeHotspot> {
    native::NativeHotspot::new()
        .and_then(|h| h.start(ssid, pass, band).map(|_| h))
}

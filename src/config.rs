use serde::Deserialize;
use std::fs;
use std::path::Path;

/// Top-level application configuration loaded from a TOML file.
#[derive(Debug, Deserialize, Clone, Default)]
pub struct AppConfig {
    #[serde(default, rename = "transport")]
    pub transports: Vec<TransportConfig>,
}

/// Per-transport configuration entry.
#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum TransportConfig {
    Can(CanConfig),
    Udp(UdpConfig),
    Serial(SerialConfig),
}

/// CAN transport configuration (SocketCAN or gs_usb).
#[derive(Debug, Deserialize, Clone)]
pub struct CanConfig {
    /// Human-readable name for log messages and REPL commands.
    pub name: String,
    /// Which CAN backend to use.
    pub driver: CanDriver,
    /// Cyphal node ID for this transport (0–127).
    pub node_id: u8,
    /// SocketCAN interface name (e.g. "can0"). Required when `driver = "socketcan"`.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub interface: Option<String>,
    /// gs_usb device index (0-based) when multiple adapters are attached.
    #[serde(default)]
    pub usb_index: u32,
    /// Override USB vendor ID (hex string such as "0x1d50"). Optional.
    pub vid: Option<String>,
    /// Override USB product ID (hex string such as "0x606f"). Optional.
    pub pid: Option<String>,
    /// CAN bus bitrate in bit/s (e.g. 1000000 for 1 Mbit/s). Defaults to 1 Mbit/s.
    #[serde(default = "default_bitrate")]
    pub bitrate: u32,
    /// Enable CAN FD mode. Defaults to false.
    #[serde(default)]
    #[allow(dead_code)]
    pub fd: bool,
}

fn default_bitrate() -> u32 {
    1_000_000
}

/// Which CAN hardware driver to use.
#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum CanDriver {
    /// Linux SocketCAN (Linux only). Requires the `gs_usb` kernel module for USB adapters.
    Socketcan,
    /// Direct userspace gs_usb/candleLight USB driver (all platforms).
    Gsusb,
}

/// Cyphal/UDP transport configuration.
#[derive(Debug, Deserialize, Clone)]
pub struct UdpConfig {
    /// Human-readable name.
    pub name: String,
    /// Cyphal node ID for this transport (0–65534).
    pub node_id: u16,
    /// Local interface IP address to bind to (e.g. "192.168.1.50").
    pub interface: String,
    /// UDP port. Defaults to the standard Cyphal UDP port 9382.
    pub port: Option<u16>,
}

/// Cyphal/Serial transport configuration.
#[derive(Debug, Deserialize, Clone)]
pub struct SerialConfig {
    /// Human-readable name.
    pub name: String,
    /// Cyphal node ID for this transport (0–65534).
    pub node_id: u16,
    /// Serial port path (e.g. "/dev/ttyUSB0" on Linux, "/dev/cu.usbmodem…" on macOS).
    pub port: String,
    /// Serial baud rate. Defaults to 115200.
    #[serde(default = "default_baud_rate")]
    pub baud_rate: u32,
}

fn default_baud_rate() -> u32 {
    115_200
}

/// Load and parse a TOML config file.
///
/// Returns `Ok(AppConfig::default())` when `path` is `None`.
pub fn load_config(path: Option<&Path>) -> Result<AppConfig, String> {
    let Some(path) = path else {
        return Ok(AppConfig::default());
    };

    let content = fs::read_to_string(path)
        .map_err(|e| format!("Cannot read config {}: {e}", path.display()))?;

    toml::from_str(&content).map_err(|e| format!("Config parse error in {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let cfg = AppConfig::default();
        assert!(cfg.transports.is_empty());
    }

    #[test]
    fn test_parse_can_gsusb() {
        let toml = r#"
[[transport]]
type     = "can"
name     = "can-usb"
driver   = "gsusb"
node_id  = 42
bitrate  = 1000000
"#;
        let cfg: AppConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.transports.len(), 1);
        match &cfg.transports[0] {
            TransportConfig::Can(c) => {
                assert_eq!(c.name, "can-usb");
                assert_eq!(c.driver, CanDriver::Gsusb);
                assert_eq!(c.node_id, 42);
            }
            _ => panic!("Expected CAN transport"),
        }
    }

    #[test]
    fn test_parse_udp() {
        let toml = r#"
[[transport]]
type      = "udp"
name      = "udp-primary"
node_id   = 100
interface = "127.0.0.1"
"#;
        let cfg: AppConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.transports.len(), 1);
        match &cfg.transports[0] {
            TransportConfig::Udp(u) => {
                assert_eq!(u.node_id, 100);
                assert_eq!(u.interface, "127.0.0.1");
                assert_eq!(u.port, None);
            }
            _ => panic!("Expected UDP transport"),
        }
    }

    #[test]
    fn test_parse_serial() {
        let toml = r#"
[[transport]]
type      = "serial"
name      = "serial-primary"
node_id   = 200
port      = "/dev/ttyUSB0"
baud_rate = 115200
"#;
        let cfg: AppConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.transports.len(), 1);
        match &cfg.transports[0] {
            TransportConfig::Serial(s) => {
                assert_eq!(s.node_id, 200);
                assert_eq!(s.port, "/dev/ttyUSB0");
                assert_eq!(s.baud_rate, 115_200);
            }
            _ => panic!("Expected Serial transport"),
        }
    }
}

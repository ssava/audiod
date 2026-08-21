use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Default, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub shim: ShimConfig,
}

#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_socket_path")]
    pub socket_path: String,
    #[serde(default = "default_volume")]
    pub volume: f64,
    /// Rate the mixer runs the backend at; all clients are resampled to it.
    #[serde(default = "default_mix_rate")]
    pub mix_rate: u32,
    /// Maximum concurrent PCM clients; further connections are rejected.
    #[serde(default = "default_max_clients")]
    pub max_clients: usize,
    /// Per-client server-side ring capacity in bytes (S16LE stereo frames).
    #[serde(default = "default_client_ring_bytes")]
    pub client_ring_bytes: usize,
    /// Client→mix-rate resampler: "sinc" (default) or "linear" (debug/A-B).
    #[serde(default = "default_resampler")]
    pub resampler: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            socket_path: default_socket_path(),
            volume: default_volume(),
            mix_rate: default_mix_rate(),
            max_clients: default_max_clients(),
            client_ring_bytes: default_client_ring_bytes(),
            resampler: default_resampler(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ShimConfig {
    #[serde(default = "default_ring_buffer_size")]
    pub ring_buffer_size: usize,
}

impl Default for ShimConfig {
    fn default() -> Self {
        ShimConfig {
            ring_buffer_size: default_ring_buffer_size(),
        }
    }
}

fn default_socket_path() -> String {
    crate::SOCKET_PATH_STR.to_string()
}

fn default_ring_buffer_size() -> usize {
    1_048_576
}

fn default_volume() -> f64 {
    1.0
}

fn default_mix_rate() -> u32 {
    48000
}

fn default_max_clients() -> usize {
    8
}

fn default_client_ring_bytes() -> usize {
    65536
}

fn default_resampler() -> String {
    "sinc".to_string()
}

static CONFIG: std::sync::OnceLock<Config> = std::sync::OnceLock::new();

pub fn load_config() -> &'static Config {
    CONFIG.get_or_init(|| {
        let path = std::env::var("AUDIOD_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
                PathBuf::from(home).join(".config/audiod/config.toml")
            });
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    })
}

use serde::Deserialize;
use std::collections::HashMap;

#[derive(Deserialize, Clone, Debug)]
pub struct Profile {
    pub name_pattern: String,
    pub iso_pattern: String,
    pub min: u32,
    pub max: u32,
}

#[derive(Deserialize, Debug)]
pub struct AppConfig {
    pub profiles: HashMap<String, Profile>,
    pub check_interval: u64,
    pub window_seconds: usize,
}

pub fn load() -> AppConfig {
    let content = std::fs::read_to_string("/etc/vcpu-agent/config.toml")
        .expect("Fichier /etc/vcpu-agent/config.toml manquant !");
    toml::from_str(&content).expect("TOML invalide")
}

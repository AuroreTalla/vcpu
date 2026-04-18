use serde::Deserialize;
use std::collections::HashMap;

#[derive(Deserialize, Clone, Debug)]
pub struct Profile {
    pub iso_pattern: String,   // Seul champ obligatoire maintenant
    pub min: u32,
    pub max: u32,
}

#[derive(Deserialize, Debug)]
pub struct AppConfig {
    pub profiles: HashMap<String, Profile>,
    pub check_interval: u64,
    pub window_seconds: usize,
    #[serde(default = "default_ratio")]
    pub cpu_overcommit_ratio: f64,    // pour virtualisation imbriquée
    #[serde(default = "default_detresse")]
    pub seuil_detresse: f64,
    #[serde(default = "default_donneuse")]
    pub seuil_donneuse: f64,
    #[serde(default = "default_duree")]
    pub duree_avant_action: usize,
}

fn default_ratio()    -> f64   { 1.0 }
fn default_detresse() -> f64   { 0.90 }
fn default_donneuse() -> f64   { 0.30 }
fn default_duree()    -> usize { 5 }

pub fn load() -> AppConfig {
    let content = std::fs::read_to_string("/etc/vcpu-agent/config.toml")
        .expect("Fichier /etc/vcpu-agent/config.toml manquant !");
    toml::from_str(&content).expect("TOML invalide")
}

use serde::Deserialize;
use std::collections::HashMap;

#[derive(Deserialize, Clone, Debug)]
pub struct Profile {
    pub iso_pattern: String,
    pub min: u32,
    pub max: u32,
}

#[derive(Deserialize, Debug)]
pub struct AppConfig {
    pub profiles: HashMap<String, Profile>,

    /// Intervalle entre deux cycles de mesure (secondes).
    pub check_interval: u64,

    /// Capacité du deque de mesures (nb d'échantillons conservés).
    #[serde(default = "default_window_size")]
    pub window_size: usize,

    /// Nb minimum de mesures dépassant le seuil pour déclencher une action.
    /// Doit être ≤ window_size.
    #[serde(default = "default_min_samples_before_action")]
    pub min_samples_before_action: usize,

    #[serde(default = "default_ratio")]
    pub cpu_overcommit_ratio: f64,

    #[serde(default = "default_detresse")]
    pub seuil_detresse: f64,

    #[serde(default = "default_donneuse")]
    pub seuil_donneuse: f64,

    #[serde(default = "default_duree")]
    pub duree_avant_action: usize,

    #[serde(default = "default_migration_cooldown")]
    pub migration_cooldown_seconds: u64,

    #[serde(default = "default_distress_before_mig")]
    pub distress_before_migration: usize,

    /// Délai (en cycles) à ignorer après un changement de vCPUs avant de
    /// reprendre les décisions. Évite les faux positifs juste après un set.
    #[serde(default = "default_vcpu_change_cooldown")]
    pub vcpu_change_cooldown_cycles: usize,
}

// ── Valeurs par défaut ────────────────────────────────────────────────────────

fn default_window_size()              -> usize { 2   }
fn default_min_samples_before_action() -> usize { 1   }
fn default_migration_cooldown()        -> u64   { 100 }
fn default_distress_before_mig()       -> usize { 4   }
fn default_ratio()                     -> f64   { 1.0 }
fn default_detresse()                  -> f64   { 0.90 }
fn default_donneuse()                  -> f64   { 0.30 }
fn default_duree()                     -> usize { 2   }
fn default_vcpu_change_cooldown()      -> usize { 2   }

pub fn load() -> AppConfig {
    let content = std::fs::read_to_string("/etc/vcpu-agent/config.toml")
        .expect("Fichier /etc/vcpu-agent/config.toml manquant !");
    let cfg: AppConfig = toml::from_str(&content).expect("TOML invalide");

    // Validation : min_samples_before_action ne peut pas dépasser window_size
    assert!(
        cfg.min_samples_before_action <= cfg.window_size,
        "min_samples_before_action ({}) doit être ≤ window_size ({})",
        cfg.min_samples_before_action,
        cfg.window_size
    );

    cfg
}

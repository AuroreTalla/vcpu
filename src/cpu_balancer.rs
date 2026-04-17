use crate::config::{AppConfig, Profile};
use crate::logger;
use crate::proxmox::*;
use crate::vm_recognizer::detect_profile;

use std::collections::{HashMap, VecDeque};
use std::thread;
use std::time::Duration;

// ─────────────────────────────
// SEUILS
// ─────────────────────────────
const SEUIL_DETRESSE: f64 = 0.80;
const SEUIL_RETOUR:   f64 = 0.30;

// ─────────────────────────────
// FONCTION PRINCIPALE
// ─────────────────────────────
pub fn run(config: AppConfig) {
    logger::log_message("=== Agent vCPU Balancer (version stable test) ===");

    let mut history: HashMap<u32, VecDeque<f64>> = HashMap::new();

    loop {
        let vms = get_all_vms();

        let mut distressed: Vec<(u32, Profile)> = vec![];
        let mut donors: Vec<(u32, Profile)> = vec![];

        // ─────────────────────────────
        // 1. COLLECTE + ANALYSE
        // ─────────────────────────────
        for &vmid in &vms {
            let name = match get_vm_name(vmid) {
                Some(n) => n,
                None => continue,
            };

            let profile = match detect_profile(vmid, &name, &config.profiles) {
                Some(p) => p,
                None => continue,
            };

            let hist = history
                .entry(vmid)
                .or_insert_with(|| VecDeque::with_capacity(config.window_seconds));

            let status = match get_vm_status(vmid) {
                Some(s) => s,
                None => continue,
            };

            let cpu_total = status["cpu"].as_f64().unwrap_or(0.0);
            let current_vcpus = get_current_vcpus(vmid).unwrap_or(1);

            let usage = if current_vcpus > 0 {
                (cpu_total / current_vcpus as f64).clamp(0.0, 1.0)
            } else {
                0.0
            };

            hist.push_back(usage);
            if hist.len() > config.window_seconds {
                hist.pop_front();
            }

            if hist.len() < config.window_seconds {
                continue;
            }

            let avg = hist.iter().sum::<f64>() / hist.len() as f64;

            logger::log_debug(&format!(
                "VM {} | vCPU={} | usage={:.1}%",
                vmid,
                current_vcpus,
                avg * 100.0
            ));

            // 🔴 DETRESSE
            if avg > SEUIL_DETRESSE && current_vcpus < profile.max as u32 {
                distressed.push((vmid, profile.clone()));

                logger::log_message(&format!(
                    "🚨 VM {} en DETRESSE ({:.1}%)",
                    vmid, avg * 100.0
                ));
            }

            // 🟢 DONNEUSE
            if avg < SEUIL_RETOUR && current_vcpus > profile.min as u32 {
                donors.push((vmid, profile.clone()));

                logger::log_message(&format!(
                    "💚 VM {} sous-utilisée ({:.1}%)",
                    vmid, avg * 100.0
                ));
            }
        }

        // ─────────────────────────────
        // 2. RESTITUTION (PRIORITAIRE)
        // ─────────────────────────────
        for (vmid, profile) in &donors {
            let current = get_current_vcpus(*vmid).unwrap_or(0);

            if current > profile.min as u32 {
                let new_vcpus = current - 1;

                logger::log_message(&format!(
                    "🔻 VM {} rend 1 vCPU ({} → {})",
                    vmid, current, new_vcpus
                ));

                apply_vcpus(*vmid, new_vcpus);

                // ⚠️ on ne rend qu'un seul par cycle
                break;
            }
        }

        // ─────────────────────────────
        // 3. REBALANCING (EMPRUNT)
        // ─────────────────────────────
        let mut transfers = 0;
        let max_transfers = 2;

        while !distressed.is_empty() && transfers < max_transfers {

            let (d_vm, d_profile) = distressed.remove(0);

            let d_current = get_current_vcpus(d_vm).unwrap_or(0);

            if d_current >= d_profile.max as u32 {
                continue;
            }

            // chercher un donneur
            if let Some((donor_vm, donor_profile)) = donors.pop() {

                let donor_current = get_current_vcpus(donor_vm).unwrap_or(0);

                if donor_current > donor_profile.min as u32 {

                    let new_donor = donor_current - 1;
                    let new_d = d_current + 1;

                    logger::log_message(&format!(
                        "🔄 TRANSFERT: VM{} ({}→{}) → VM{} ({}→{})",
                        donor_vm, donor_current, new_donor,
                        d_vm, d_current, new_d
                    ));

                    apply_vcpus(donor_vm, new_donor);
                    apply_vcpus(d_vm, new_d);

                    transfers += 1;

                    thread::sleep(Duration::from_secs(1));
                }
            }
        }

        thread::sleep(Duration::from_secs(config.check_interval));
    }
}

// ─────────────────────────────
// APPLIQUER VCPU (simple)
// ─────────────────────────────
fn apply_vcpus(vmid: u32, target: u32) {
    let _ = run_command(&format!("qm set {} --vcpus {}", vmid, target));
}

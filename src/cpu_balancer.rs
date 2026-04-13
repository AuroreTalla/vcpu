use crate::config::{AppConfig, Profile};
use crate::logger;
use crate::proxmox::*;
use crate::vm_recognizer::detect_profile;
use std::collections::{HashMap, VecDeque};
use std::time::Duration;
use std::thread;

pub fn run(config: AppConfig) {
    logger::log_message("=== Agent vCPU Balancer (agent-vcpu) démarré - reconnaissance ISO + nom ===");

    let mut history: HashMap<u32, VecDeque<f64>> = HashMap::new();

    loop {
        let vms = get_all_vms();
        let mut distressed: Vec<(u32, Profile)> = vec![];
        let mut donors: Vec<(u32, Profile)> = vec![];

        for &vmid in &vms {
            let name = match get_vm_name(vmid) {
                Some(n) => n,
                None => continue,
            };
            let iso_filename = get_iso_filename(vmid);
            let profile = match detect_profile(&name, iso_filename.as_deref(), &config.profiles) {
                Some(p) => p,
                None => continue,
            };

            history.entry(vmid).or_insert_with(|| VecDeque::with_capacity(config.window_seconds));

            let status = match get_vm_status(vmid) {
                Some(s) => s,
                None => continue,
            };
            let cpu = status["cpu"].as_f64().unwrap_or(0.0);
            let vcpus = match get_current_vcpus(vmid) {
                Some(v) => v,
                None => continue,
            };

            let ratio = if vcpus > 0 { cpu / vcpus as f64 } else { 0.0 };

            let hist = history.get_mut(&vmid).unwrap();
            hist.push_back(ratio);
            if hist.len() > config.window_seconds {
                hist.pop_front();
            }

            if hist.len() == config.window_seconds {
                if hist.iter().all(|&x| x > 0.90) {
                    distressed.push((vmid, profile.clone()));
                }
                if hist.iter().all(|&x| x < 0.30) && vcpus > profile.min {
                    donors.push((vmid, profile.clone()));
                }
            }
        }

        while !distressed.is_empty() && !donors.is_empty() {
            let (d_vm, d_profile) = distressed.remove(0);
            let (donor_vm, donor_profile) = donors.remove(0);

            let d_current = get_current_vcpus(d_vm).unwrap_or(0);
            let donor_current = get_current_vcpus(donor_vm).unwrap_or(0);

            if d_current < d_profile.max && donor_current > donor_profile.min {
                let new_donor = donor_current - 1;
                let new_d = d_current + 1;

                logger::log_message(&format!(
                    "Transfert 1 vCPU → VM{} ({}→{}) ← VM{} ({}→{})",
                    donor_vm, donor_current, new_donor, d_vm, d_current, new_d
                ));

                let _ = run_command(&format!("qm set {} --vcpus {}", donor_vm, new_donor));
                let _ = run_command(&format!("qm set {} --vcpus {}", d_vm, new_d));
                thread::sleep(Duration::from_secs(1));
            }
        }

        thread::sleep(Duration::from_secs(config.check_interval));
    }
}

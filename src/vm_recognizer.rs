use crate::config::Profile;
use crate::proxmox::{get_vm_ostype, get_iso_filename};
use std::collections::HashMap;
use crate::logger;

pub fn detect_profile(
    vmid: u32,
    vm_name: &str,
    profiles: &HashMap<String, Profile>,
) -> Option<Profile> {
    logger::log_debug(&format!("=== Détection OS pour VM {} ('{}') ===", vmid, vm_name));

    let ostype = get_vm_ostype(vmid);
    let iso = get_iso_filename(vmid);

    logger::log_debug(&format!("→ ostype : {:?}", ostype));
    logger::log_debug(&format!("→ ISO    : {:?}", iso));

    // PRIORITÉ 1 : Détection par ISO (méthode principale)
    if let Some(iso_name) = &iso {
        let iso_lower = iso_name.to_lowercase();
        for (profile_name, profile) in profiles {
            if !profile.iso_pattern.is_empty() 
                && iso_lower.contains(&profile.iso_pattern.to_lowercase()) {
                
                logger::log_message(&format!(
                    "OS DÉTECTÉ via ISO → VM {} | Profil '{}' | ISO '{}' | min={} | max={}",
                    vmid, profile_name, iso_name, profile.min, profile.max
                ));
                return Some(profile.clone());
            }
        }
    }

    // PRIORITÉ 2 : Fallback par ostype (Linux générique)
    if let Some(os) = &ostype {
        if os.starts_with("l2") || os.to_lowercase().contains("linux") {
            if let Some(linux_profile) = profiles.get("linux") {
                logger::log_message(&format!(
                    "OS DÉTECTÉ via ostype → VM {} | Profil 'linux' | ostype '{}' | min={} | max={}",
                    vmid, os, linux_profile.min, linux_profile.max
                ));
                return Some(linux_profile.clone());
            }
        }
    }

    logger::log_debug(&format!("Aucun profil trouvé pour VM {}", vmid));
    None
}

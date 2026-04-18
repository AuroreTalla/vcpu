use crate::config::Profile;
use crate::proxmox::{get_vm_ostype, get_iso_filename};
use std::collections::HashMap;

pub fn detect_profile(vmid: u32, vm_name: &str, profiles: &HashMap<String, Profile>) -> Option<Profile> {
    // PRIORITÉ 1 : par ISO
    if let Some(iso) = get_iso_filename(vmid) {
        let iso_lower = iso.to_lowercase();
        for (_profile_name, profile) in profiles {
            // Ignorer les patterns vides (profil linux générique)
            if !profile.iso_pattern.is_empty()
                && iso_lower.contains(&profile.iso_pattern.to_lowercase())
            {
                return Some(profile.clone());
            }
        }
    }

    // PRIORITÉ 2 : par ostype
    if let Some(os) = get_vm_ostype(vmid) {
        if os.starts_with("l2") || os.to_lowercase().contains("linux") {
            if let Some(profile) = profiles.get("linux") {
                return Some(profile.clone());
            }
        }
    }

    // PRIORITÉ 3 : par nom de VM (patterns non vides uniquement)
    for (_profile_name, profile) in profiles {
        if !profile.iso_pattern.is_empty()
            && vm_name.to_lowercase().contains(&profile.iso_pattern.to_lowercase())
        {
            return Some(profile.clone());
        }
    }

    None
}

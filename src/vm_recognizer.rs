use crate::config::Profile;
use std::collections::HashMap;

pub fn detect_profile(
    name: &str,
    iso_filename: Option<&str>,
    profiles: &HashMap<String, Profile>,
) -> Option<Profile> {
    let lower_name = name.to_lowercase();

    // Priorité 1 : reconnaissance par nom de VM
    for profile in profiles.values() {
        if !profile.name_pattern.is_empty() && lower_name.contains(&profile.name_pattern.to_lowercase()) {
            return Some(profile.clone());
        }
    }

    // Priorité 2 : reconnaissance par nom du fichier ISO (plus fiable)
    if let Some(iso) = iso_filename {
        let lower_iso = iso.to_lowercase();
        for profile in profiles.values() {
            if !profile.iso_pattern.is_empty() && lower_iso.contains(&profile.iso_pattern.to_lowercase()) {
                return Some(profile.clone());
            }
        }
    }
    None
}

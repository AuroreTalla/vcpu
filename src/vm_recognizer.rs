use crate::config::Profile;
use crate::proxmox::get_vm_config;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread;

pub fn detect_profile(vmid: u32, profiles: &HashMap<String, Profile>) -> Option<Profile> {

    let cfg = match get_vm_config(vmid) {
        Some(c) => Arc::new(c),
        None    => {
            crate::logger::log_message(&format!(
                "⚠️  VM {} → get_vm_config() a échoué, VM ignorée", vmid
            ));
            return None;
        }
    };

    // Résultat partagé entre les threads : (confiance, nom_profil, profil)
    // confiance : 0=ISO (meilleur), 1=tag, 2=ostype — le plus bas gagne
    let result: Arc<Mutex<Option<(u8, String, Profile)>>> = Arc::new(Mutex::new(None));

    let mut handles = vec![];

    // ── Thread 1 : ISO ────────────────────────────────────────────────────────
    {
        let cfg      = Arc::clone(&cfg);
        let profiles = profiles.clone();
        let result   = Arc::clone(&result);
        handles.push(thread::spawn(move || {
            if let Some(obj) = cfg.as_object() {
                for (_key, value) in obj {
                    if let Some(s) = value.as_str() {
                        if let Some(pos) = s.find("iso/") {
                            let iso = s[pos + 4..].split(',').next().unwrap_or("").to_string();
                            let iso_lower = iso.to_lowercase();
                            for (nom, profil) in &profiles {
                                if !profil.iso_pattern.is_empty()
                                    && iso_lower.contains(&profil.iso_pattern.to_lowercase())
                                {
                                    let mut r = result.lock().unwrap();
                                    // Garder uniquement si meilleure confiance
                                    let meilleur = r.as_ref()
                                        .map(|(c, _, _)| 0 < *c)
                                        .unwrap_or(true);
                                    if meilleur {
                                        *r = Some((0, nom.clone(), profil.clone()));
                                        crate::logger::log_message(&format!(
                                            "🔎 VM {} → profil '{}' [ISO '{}']",
                                            vmid, nom, iso
                                        ));
                                    }
                                    break;
                                }
                            }
                            break; // un seul ISO par VM
                        }
                    }
                }
            }
        }));
    }

    // ── Thread 2 : Tags ───────────────────────────────────────────────────────
    {
        let cfg      = Arc::clone(&cfg);
        let profiles = profiles.clone();
        let result   = Arc::clone(&result);
        handles.push(thread::spawn(move || {
            if let Some(tags_str) = cfg["tags"].as_str() {
                let tags_str = tags_str.to_string();
                for tag in tags_str.split(';').map(|t| t.trim()).filter(|t| !t.is_empty()) {
                    let tag_lower = tag.to_lowercase();
                    for (nom, profil) in &profiles {
                        if !profil.iso_pattern.is_empty()
                            && tag_lower.contains(&profil.iso_pattern.to_lowercase())
                        {
                            let mut r = result.lock().unwrap();
                            let meilleur = r.as_ref()
                                .map(|(c, _, _)| 1 < *c)
                                .unwrap_or(true);
                            if meilleur {
                                *r = Some((1, nom.clone(), profil.clone()));
                                crate::logger::log_message(&format!(
                                    "🔎 VM {} → profil '{}' [Tag '{}']",
                                    vmid, nom, tags_str
                                ));
                            }
                            break;
                        }
                    }
                }
            }
        }));
    }

    // ── Thread 3 : ostype ─────────────────────────────────────────────────────
    {
        let cfg      = Arc::clone(&cfg);
        let profiles = profiles.clone();
        let result   = Arc::clone(&result);
        handles.push(thread::spawn(move || {
            if let Some(os) = cfg["ostype"].as_str() {
                let profil_nom = ostype_to_profile_name(os);
                if let Some(profil) = profiles.get(profil_nom) {
                    let mut r = result.lock().unwrap();
                    let meilleur = r.as_ref()
                        .map(|(c, _, _)| 2 < *c)
                        .unwrap_or(true);
                    if meilleur {
                        *r = Some((2, profil_nom.to_string(), profil.clone()));
                        crate::logger::log_message(&format!(
                            "🔎 VM {} → profil '{}' [ostype='{}']",
                            vmid, profil_nom, os
                        ));
                    }
                }
            }
        }));
    }

    // Attendre tous les threads
    for h in handles { let _ = h.join(); }

    // ── Résultat ──────────────────────────────────────────────────────────────
    let r = Arc::try_unwrap(result).unwrap().into_inner().unwrap();

    if let Some((_, nom, profil)) = r {
        return Some(profil);
    }

    // ── Fallback linux ────────────────────────────────────────────────────────
    if let Some(profil) = profiles.get("linux") {
        crate::logger::log_message(&format!(
            "🔎 VM {} → profil 'linux' [fallback]", vmid
        ));
        return Some(profil.clone());
    }

    crate::logger::log_message(&format!(
        "⚠️  VM {} → aucun profil détecté (pas de profil linux défini non plus)", vmid
    ));
    None
}

fn ostype_to_profile_name(ostype: &str) -> &'static str {
    match ostype {
        "l24" | "l26"                                       => "linux",
        "win7" | "win8" | "win10" | "win11"
        | "wxp" | "w2k" | "w2k3" | "w2k8" | "wvista"      => "windows",
        _                                                   => "linux",
    }
}

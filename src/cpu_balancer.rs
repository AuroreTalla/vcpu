use crate::config::{AppConfig, Profile};
use crate::logger;
use crate::proxmox::*;
use crate::vm_recognizer::detect_profile;
use std::collections::{HashMap, VecDeque};
use std::thread;
use std::time::Duration;

// ─── État par VM ──────────────────────────────────────────────────────────────

#[derive(Debug)]
struct VMState {
    history:          VecDeque<f64>,
    distress_counter: u32,
    low_counter:      u32,
}

impl VMState {
    fn new(capacity: usize) -> Self {
        VMState {
            history:          VecDeque::with_capacity(capacity),
            distress_counter: 0,
            low_counter:      0,
        }
    }
}

// ─── Affichage état ───────────────────────────────────────────────────────────

fn log_separator() {
    logger::log_message("─────────────────────────────────────────────────");
}

fn log_etat_vms(
    vm_avg_usage:     &HashMap<u32, f64>,
    vm_profiles:      &HashMap<u32, Profile>,
    vm_current_vcpus: &HashMap<u32, u32>,
    vm_states:        &HashMap<u32, VMState>,
    pending_loans:    &HashMap<u32, (u32, u32)>,
    seuil_detresse:   f64,
    seuil_donneuse:   f64,
) {
    let mut vmids: Vec<u32> = vm_avg_usage.keys().cloned().collect();
    vmids.sort();

    for vmid in vmids {
        let avg     = vm_avg_usage[&vmid];
        let profile = &vm_profiles[&vmid];
        let vcpus   = *vm_current_vcpus.get(&vmid).unwrap_or(&profile.min);
        let state   = &vm_states[&vmid];
        let loaned  = pending_loans.get(&vmid).map(|l| l.1).unwrap_or(0);

        let statut = if avg >= seuil_detresse && vcpus >= profile.max {
            "🔴 SATURÉE "
        } else if avg >= seuil_detresse {
            "🔴 DÉTRESSE"
        } else if loaned > 0 {
            "🟠 REMB.   "
        } else if avg <= seuil_donneuse {
            "🟢 REPOS   "
        } else {
            "🟡 NORMAL  "
        };

        let pret_str = if loaned > 0 {
            let preteur = pending_loans[&vmid].0;
            if preteur == 0 {
                format!(" [doit rendre {} vCPU(s) à l'hôte]", loaned)
            } else {
                format!(" [doit rendre {} vCPU(s) à VM {}]", loaned, preteur)
            }
        } else {
            String::new()
        };

        logger::log_message(&format!(
            "{} VM {:>3} | CPU={:>5.1}% | vCPUs={} |(max={} min={})| {}  | det={} repos={}",
            statut, vmid, avg * 100.0,
            vcpus, profile.max, profile.min,
            pret_str,
            state.distress_counter,
            state.low_counter
        ));
    }
    log_separator();
}

// ─── Point d'entrée ───────────────────────────────────────────────────────────

pub fn run(config: AppConfig) {
    logger::log_message("╔══════════════════════════════════════════════════╗");
    logger::log_message("║         Agent vCPU Balancer — Démarrage          ║");
    logger::log_message("╚══════════════════════════════════════════════════╝");
    logger::log_message(&format!(
        "Seuil détresse: {:.0}% | Seuil repos: {:.0}% | Action après: {} ticks | Intervalle: {}s",
        config.seuil_detresse * 100.0,
        config.seuil_donneuse * 100.0,
        config.duree_avant_action,
        config.check_interval,
    ));

    let mut vm_states:     HashMap<u32, VMState>    = HashMap::new();
    // pending_loans[emprunteur] = (preteur_id, nb_vcpus)
    // preteur_id = 0 → hôte (overcommit)
    let mut pending_loans: HashMap<u32, (u32, u32)> = HashMap::new();

    loop {
        let vms = get_all_vms();
        if vms.is_empty() {
            logger::log_message("⚠️  Aucune VM trouvée, nouvel essai dans 5s...");
            thread::sleep(Duration::from_secs(5));
            continue;
        }

        // ── Étape 1 : Collecte ───────────────────────────────────────────────
        let mut vm_profiles:      HashMap<u32, Profile> = HashMap::new();
        let mut vm_avg_usage:     HashMap<u32, f64>     = HashMap::new();
        let mut vm_current_vcpus: HashMap<u32, u32>     = HashMap::new();

        for &vmid in &vms {
            let name = match get_vm_name(vmid) {
                Some(n) => n,
                None    => continue,
            };
            let profile = match detect_profile(vmid, &name, &config.profiles) {
                Some(p) => p,
                None    => continue,
            };

            let current_vcpus = get_current_vcpus(vmid).unwrap_or(profile.min);
            vm_current_vcpus.insert(vmid, current_vcpus);
            vm_profiles.insert(vmid, profile.clone());

            let usage = get_vm_cpu_usage(vmid);

            let state = vm_states
                .entry(vmid)
                .or_insert_with(|| VMState::new(config.window_seconds));

            state.history.push_back(usage);
            if state.history.len() > config.window_seconds {
                state.history.pop_front();
            }

            if state.history.len() == config.window_seconds {
                let avg = state.history.iter().sum::<f64>() / state.history.len() as f64;
                vm_avg_usage.insert(vmid, avg);
            }
        }

        if vm_avg_usage.is_empty() {
            thread::sleep(Duration::from_secs(config.check_interval));
            continue;
        }

        // ── Étape 2 : Compteurs de ticks ─────────────────────────────────────
        for (&vmid, &avg) in &vm_avg_usage {
            let profile = &vm_profiles[&vmid];
            let vcpus   = *vm_current_vcpus.get(&vmid).unwrap_or(&profile.min);
            let state   = vm_states.get_mut(&vmid).unwrap();

            if avg >= config.seuil_detresse && vcpus < profile.max {
                state.distress_counter += 1;
                state.low_counter       = 0;
            } else if avg <= config.seuil_donneuse && vcpus > profile.min {
                // S'applique aussi aux VMs qui ont emprunté → permet le remboursement
                state.low_counter      += 1;
                state.distress_counter  = 0;
            } else {
                state.distress_counter = 0;
                state.low_counter      = 0;
            }
        }

        // ── Affichage état — une seule fois par cycle ─────────────────────────
        log_etat_vms(
            &vm_avg_usage, &vm_profiles, &vm_current_vcpus,
            &vm_states, &pending_loans,
            config.seuil_detresse, config.seuil_donneuse,
        );

        // ── Étape 3 : Remboursements ──────────────────────────────────────────
        for (&emprunteur_id, &(preteur_id, loaned)) in &pending_loans.clone() {
            if loaned == 0 { continue; }

            let low_ticks = vm_states.get(&emprunteur_id)
                .map(|s| s.low_counter)
                .unwrap_or(0);

            if (low_ticks as usize) < config.duree_avant_action { continue; }

            let vcpus_emp = *vm_current_vcpus.get(&emprunteur_id).unwrap_or(&0);
            let profile_emp = match vm_profiles.get(&emprunteur_id) {
                Some(p) => p,
                None    => continue,
            };

            if vcpus_emp <= profile_emp.min {
                pending_loans.remove(&emprunteur_id);
                logger::log_message(&format!(
                    "ℹ️  VM {} déjà au minimum ({} vCPUs) — prêt soldé",
                    emprunteur_id, vcpus_emp
                ));
                continue;
            }

            let new_emp = vcpus_emp - 1;

            if preteur_id == 0 {
                // Overcommit hôte : libérer sans rendre à personne
                if set_vm_vcpus(emprunteur_id, new_emp).is_some() {
                    vm_current_vcpus.insert(emprunteur_id, new_emp);
                    if let Some(loan) = pending_loans.get_mut(&emprunteur_id) {
                        loan.1 -= 1;
                    }
                    if pending_loans.get(&emprunteur_id).map(|l| l.1) == Some(0) {
                        pending_loans.remove(&emprunteur_id);
                    }
                    vm_states.get_mut(&emprunteur_id).unwrap().low_counter = 0;
                    logger::log_message(&format!(
                        "↩️  RETOUR HÔTE : VM {} libère 1 vCPU ({} → {} vCPUs) | reste à rendre: {}",
                        emprunteur_id, vcpus_emp, new_emp,
                        pending_loans.get(&emprunteur_id).map(|l| l.1).unwrap_or(0)
                    ));
                } else {
                    logger::log_message(&format!(
                        "❌ Échec retour hôte VM {}", emprunteur_id
                    ));
                }
            } else {
                // Prêt VM→VM : rendre à la VM prêteuse
                let vcpus_pret = *vm_current_vcpus.get(&preteur_id).unwrap_or(&0);

                let profile_pret = match vm_profiles.get(&preteur_id) {
                    Some(p) => p,
                    None    => {
                        // Prêteuse absente (éteinte?) → libérer quand même
                        if set_vm_vcpus(emprunteur_id, new_emp).is_some() {
                            vm_current_vcpus.insert(emprunteur_id, new_emp);
                            if let Some(loan) = pending_loans.get_mut(&emprunteur_id) {
                                loan.1 -= 1;
                            }
                            if pending_loans.get(&emprunteur_id).map(|l| l.1) == Some(0) {
                                pending_loans.remove(&emprunteur_id);
                            }
                            vm_states.get_mut(&emprunteur_id).unwrap().low_counter = 0;
                            logger::log_message(&format!(
                                "↩️  RETOUR (VM {} absente) : VM {} libère 1 vCPU ({} → {} vCPUs)",
                                preteur_id, emprunteur_id, vcpus_emp, new_emp
                            ));
                        }
                        continue;
                    }
                };

                let new_pret = vcpus_pret + 1;

                // Si prêteuse au max → libérer sans lui rendre (elle n'en a pas besoin)
                let rendre_a_preteur = new_pret <= profile_pret.max;

                let ok_emp = set_vm_vcpus(emprunteur_id, new_emp).is_some();
                let ok_pret = if rendre_a_preteur {
                    set_vm_vcpus(preteur_id, new_pret).is_some()
                } else {
                    true // on libère sans rendre à la prêteuse
                };

                if ok_emp && ok_pret {
                    vm_current_vcpus.insert(emprunteur_id, new_emp);
                    if rendre_a_preteur {
                        vm_current_vcpus.insert(preteur_id, new_pret);
                    }
                    if let Some(loan) = pending_loans.get_mut(&emprunteur_id) {
                        loan.1 -= 1;
                    }
                    if pending_loans.get(&emprunteur_id).map(|l| l.1) == Some(0) {
                        pending_loans.remove(&emprunteur_id);
                    }
                    vm_states.get_mut(&emprunteur_id).unwrap().low_counter = 0;

                    if rendre_a_preteur {
                        logger::log_message(&format!(
                            "↩️  RETOUR OK : VM {} ({} → {} vCPUs) restitue à VM {} ({} → {} vCPUs) | reste: {}",
                            emprunteur_id, vcpus_emp, new_emp,
                            preteur_id, vcpus_pret, new_pret,
                            pending_loans.get(&emprunteur_id).map(|l| l.1).unwrap_or(0)
                        ));
                    } else {
                        logger::log_message(&format!(
                            "↩️  RETOUR OK : VM {} ({} → {} vCPUs) — VM {} au max, vCPU libéré | reste: {}",
                            emprunteur_id, vcpus_emp, new_emp,
                            preteur_id,
                            pending_loans.get(&emprunteur_id).map(|l| l.1).unwrap_or(0)
                        ));
                    }
                } else {
                    if ok_emp && !ok_pret {
                        let _ = set_vm_vcpus(emprunteur_id, vcpus_emp);
                        logger::log_message(&format!(
                            "↩️  Rollback VM {} (échec retour à VM {})",
                            emprunteur_id, preteur_id
                        ));
                    }
                    logger::log_message(&format!(
                        "❌ Échec retour VM {} → VM {}", emprunteur_id, preteur_id
                    ));
                }
            }
        }

        // ── Étape 4 : Prêts ──────────────────────────────────────────────────
        let mut distress_vms: Vec<(u32, f64)> = vm_avg_usage
            .iter()
            .filter(|(&vmid, &avg)| {
                let profile = &vm_profiles[&vmid];
                let vcpus   = *vm_current_vcpus.get(&vmid).unwrap_or(&profile.min);
                let ticks   = vm_states[&vmid].distress_counter as usize;
                avg >= config.seuil_detresse
                    && vcpus < profile.max
                    && ticks >= config.duree_avant_action
            })
            .map(|(&id, &avg)| (id, avg))
            .collect();

        distress_vms.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

        for (distress_id, distress_avg) in &distress_vms {
            let profile_d = &vm_profiles[distress_id];
            let vcpus_d   = *vm_current_vcpus.get(distress_id).unwrap();

            if vcpus_d >= profile_d.max {
                logger::log_message(&format!(
                    "⚠️  VM {} au plafond ({}/{} vCPUs, {:.0}%) — surcharge acceptée",
                    distress_id, vcpus_d, profile_d.max, distress_avg * 100.0
                ));
                continue;
            }

            // Chercher donneur : le plus au repos, pas lui-même emprunteur
            let donor = vm_current_vcpus
                .iter()
                .filter(|(&id, &vcpus)| {
                    if id == *distress_id { return false; }
                    let p      = match vm_profiles.get(&id) { Some(p) => p, None => return false };
                    let avg    = vm_avg_usage.get(&id).copied().unwrap_or(1.0);
                    let ticks  = vm_states.get(&id).map(|s| s.low_counter as usize).unwrap_or(0);
                    let loaned = pending_loans.get(&id).map(|l| l.1).unwrap_or(0);
                    vcpus > p.min
                        && avg <= config.seuil_donneuse
                        && ticks >= config.duree_avant_action
                        && loaned == 0  // ne pas prendre à une VM qui a déjà emprunté
                })
                .min_by(|a, b| {
                    let avg_a = vm_avg_usage.get(a.0).copied().unwrap_or(1.0);
                    let avg_b = vm_avg_usage.get(b.0).copied().unwrap_or(1.0);
                    avg_a.partial_cmp(&avg_b).unwrap()
                })
                .map(|(&id, _)| id);

            if let Some(donor_id) = donor {
                let vcpus_don    = *vm_current_vcpus.get(&donor_id).unwrap();
                let new_donor    = vcpus_don - 1;
                let new_distress = vcpus_d + 1;
                let donor_avg    = vm_avg_usage.get(&donor_id).copied().unwrap_or(0.0);

                logger::log_message(&format!(
                    "🔄 PRÊT : VM {} ({:.0}%, {} vCPUs) cède 1 vCPU → VM {} ({:.0}%, {} vCPUs)",
                    donor_id, donor_avg * 100.0, vcpus_don,
                    distress_id, distress_avg * 100.0, vcpus_d
                ));

                let ok_don  = set_vm_vcpus(donor_id, new_donor).is_some();
                let ok_dist = set_vm_vcpus(*distress_id, new_distress).is_some();

                if ok_don && ok_dist {
                    vm_current_vcpus.insert(donor_id, new_donor);
                    vm_current_vcpus.insert(*distress_id, new_distress);
                    let loan = pending_loans.entry(*distress_id).or_insert((donor_id, 0));
                    loan.1 += 1;
                    vm_states.get_mut(&donor_id).unwrap().low_counter       = 0;
                    vm_states.get_mut(distress_id).unwrap().distress_counter = 0;
                    logger::log_message(&format!(
                        "✅ PRÊT OK : VM {} → {} vCPUs | VM {} → {} vCPUs | VM {} doit {} vCPU(s) à VM {}",
                        donor_id, new_donor,
                        distress_id, new_distress,
                        distress_id, pending_loans[distress_id].1, donor_id
                    ));
                } else {
                    if ok_don && !ok_dist {
                        let _ = set_vm_vcpus(donor_id, vcpus_don);
                        logger::log_message(&format!(
                            "↩️  Rollback VM {} (échec prêt à VM {})", donor_id, distress_id
                        ));
                    }
                    logger::log_message(&format!(
                        "❌ Échec prêt VM {} → VM {}", donor_id, distress_id
                    ));
                }

            } else {
                // Pas de donneur VM disponible → overcommit hôte
                let host_cpus   = get_host_cpus();
                let total_vcpus: u32 = vm_current_vcpus.values().sum();
                let max_allowed = (host_cpus as f64 * config.cpu_overcommit_ratio) as u32;

                if total_vcpus < max_allowed {
                    let new_distress = vcpus_d + 1;
                    logger::log_message(&format!(
                        "🔄 PRÊT HÔTE : VM {} ({:.0}%) reçoit 1 vCPU hôte ({} → {} vCPUs) [{}/{}]",
                        distress_id, distress_avg * 100.0,
                        vcpus_d, new_distress,
                        total_vcpus + 1, max_allowed
                    ));
                    if set_vm_vcpus(*distress_id, new_distress).is_some() {
                        vm_current_vcpus.insert(*distress_id, new_distress);
                        let loan = pending_loans.entry(*distress_id).or_insert((0, 0));
                        loan.1 += 1;
                        vm_states.get_mut(distress_id).unwrap().distress_counter = 0;
                        logger::log_message(&format!(
                            "✅ PRÊT HÔTE OK : VM {} → {} vCPUs | doit rendre {} vCPU(s) à l'hôte",
                            distress_id, new_distress, pending_loans[distress_id].1
                        ));
                    } else {
                        logger::log_message(&format!(
                            "❌ Échec prêt hôte VM {}", distress_id
                        ));
                    }
                } else {
                    logger::log_message(&format!(
                        "⚠️  VM {} en détresse ({:.0}%) — aucun donneur, ratio hôte atteint ({}/{})",
                        distress_id, distress_avg * 100.0, total_vcpus, max_allowed
                    ));
                }
            }
        }

        thread::sleep(Duration::from_secs(config.check_interval));
    }
}

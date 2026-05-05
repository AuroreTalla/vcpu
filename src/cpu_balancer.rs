use crate::config::{AppConfig, Profile};
use crate::logger;
use crate::proxmox::*;
use crate::vm_recognizer::detect_profile;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

// ─── État par VM ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct VMState {
    history:          VecDeque<f64>,
    distress_counter: u32,
    low_counter:      u32,
}

impl VMState {
    fn new(cap: usize) -> Self {
        VMState { history: VecDeque::with_capacity(cap), distress_counter: 0, low_counter: 0 }
    }
    fn push(&mut self, v: f64, cap: usize) {
        self.history.push_back(v);
        if self.history.len() > cap { self.history.pop_front(); }
    }
    fn avg(&self) -> Option<f64> {
        if self.history.is_empty() { return None; }
        Some(self.history.iter().sum::<f64>() / self.history.len() as f64)
    }
    fn full(&self, cap: usize) -> bool { self.history.len() >= cap }
}

// ─── Prêt actif ───────────────────────────────────────────────────────────────
// Mémorise l'état initial de l'emprunteur pour savoir où revenir
#[derive(Debug, Clone)]
struct Loan {
    preteur_id:    u32,  // 0 = hôte
    nb_empruntes:  u32,  // nombre de vCPUs empruntés
    vcpus_initial: u32,  // vCPUs de l'emprunteur AVANT le premier prêt
}

// ─── Mesure parallèle ────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct VMMetric { vmid: u32, usage: f64, vcpus: u32 }

fn mesurer_toutes_vms(vmids: &[u32], profiles: &HashMap<u32, Profile>) -> Vec<VMMetric> {
    let res: Arc<Mutex<Vec<VMMetric>>> = Arc::new(Mutex::new(Vec::new()));
    let mut handles = Vec::new();
    for &vmid in vmids {
        let profile = match profiles.get(&vmid) { Some(p) => p.clone(), None => continue };
        let r = Arc::clone(&res);
        handles.push(thread::spawn(move || {
            let usage = get_vm_cpu_usage(vmid);
            let vcpus = get_current_vcpus(vmid).unwrap_or(profile.min);
            r.lock().unwrap().push(VMMetric { vmid, usage, vcpus });
        }));
    }
    for h in handles { let _ = h.join(); }
    Arc::try_unwrap(res).unwrap().into_inner().unwrap()
}

// ─── Affichage ────────────────────────────────────────────────────────────────

fn log_sep() { logger::log_message("─────────────────────────────────────────────────"); }

fn log_etat(
    vm_avg:    &HashMap<u32, f64>,
    profiles:  &HashMap<u32, Profile>,
    vcpus_map: &HashMap<u32, u32>,
    states:    &HashMap<u32, VMState>,
    loans:     &HashMap<u32, Loan>,
    sd: f64, sr: f64,
) {
    let mut ids: Vec<u32> = vm_avg.keys().cloned().collect();
    ids.sort();
    for vmid in ids {
        let avg     = vm_avg[&vmid];
        let profile = &profiles[&vmid];
        let vcpus   = *vcpus_map.get(&vmid).unwrap_or(&profile.min);
        let state   = &states[&vmid];
        let loan    = loans.get(&vmid);
        let nb_emp  = loan.map(|l| l.nb_empruntes).unwrap_or(0);

        let statut = if avg >= sd && vcpus >= profile.max { "🔴 SATURÉE " }
                     else if avg >= sd                    { "🔴 DÉTRESSE" }
                     else if nb_emp > 0                   { "🟠 REMB.   " }
                     else if avg <= sr                    { "🟢 REPOS   " }
                     else                                 { "🟡 NORMAL  " };

        let pret_str = if nb_emp > 0 {
            let init = loan.map(|l| l.vcpus_initial).unwrap_or(vcpus);
            let p    = loan.map(|l| l.preteur_id).unwrap_or(0);
            let dest = if p == 0 { "l'hôte".to_string() } else { format!("VM {}", p) };
            format!(" [doit rendre {} vCPU(s) à {} | état initial: {}]", nb_emp, dest, init)
        } else { String::new() };

        let tick_str = if state.distress_counter > 0 {
            format!("détresse depuis {}c", state.distress_counter)
        } else if state.low_counter > 0 {
            format!("repos depuis {}c", state.low_counter)
        } else { "stable".to_string() };

        logger::log_message(&format!(
            "{} VM {:>3} | CPU={:>5.1}% | vCPUs={} [min={} max={}]{} | {}",
            statut, vmid, avg * 100.0, vcpus, profile.min, profile.max, pret_str, tick_str
        ));
    }
    log_sep();
}

// ─── Point d'entrée ───────────────────────────────────────────────────────────

pub fn run(config: AppConfig) {
    logger::log_message("╔══════════════════════════════════════════════════╗");
    logger::log_message("║         Agent vCPU Balancer — Démarrage          ║");
    logger::log_message("╚══════════════════════════════════════════════════╝");
    logger::log_message(&format!(
        "Détresse: >{:.0}% | Repos: <{:.0}% | Action après: {}c | Intervalle: {}s",
        config.seuil_detresse * 100.0, config.seuil_donneuse * 100.0,
        config.duree_avant_action, config.check_interval,
    ));

    let mut vm_states: HashMap<u32, VMState> = HashMap::new();
    // loans[emprunteur_id] = Loan { preteur, nb_empruntes, vcpus_initial }
    let mut loans: HashMap<u32, Loan> = HashMap::new();

    loop {
        // ── 0 : Lister VMs actives avec tag ──────────────────────────────────
        let all = get_all_vms();
        if all.is_empty() {
            logger::log_message("⚠️  Aucune VM, retry dans 5s...");
            thread::sleep(Duration::from_secs(5));
            continue;
        }

        let mut profiles:  HashMap<u32, Profile> = HashMap::new();
        let mut actifs:    Vec<u32>              = Vec::new();

        for &vmid in &all {
            if !vm_has_agent_tag(vmid) { continue; }
            let name = match get_vm_name(vmid) { Some(n) => n, None => continue };
            if let Some(p) = detect_profile(vmid, &name, &config.profiles) {
                profiles.insert(vmid, p);
                actifs.push(vmid);
            }
        }

        if actifs.is_empty() {
            thread::sleep(Duration::from_secs(config.check_interval));
            continue;
        }

        // ── 1 : Mesures PARALLÈLES ────────────────────────────────────────────
        // Toutes les VMs mesurées simultanément → durée = 1 mesure (~500ms)
        let metriques = mesurer_toutes_vms(&actifs, &profiles);

        // ── 2 : Historique et moyennes ────────────────────────────────────────
        let mut vm_avg:  HashMap<u32, f64> = HashMap::new();
        let mut vcpus_m: HashMap<u32, u32> = HashMap::new();

        for m in &metriques {
            if profiles.get(&m.vmid).is_none() { continue; }
            vcpus_m.insert(m.vmid, m.vcpus);
            let s = vm_states.entry(m.vmid).or_insert_with(|| VMState::new(config.window_seconds));
            s.push(m.usage, config.window_seconds);
            if s.full(config.window_seconds) {
                if let Some(avg) = s.avg() { vm_avg.insert(m.vmid, avg); }
            }
        }

        if vm_avg.is_empty() {
            thread::sleep(Duration::from_secs(config.check_interval));
            continue;
        }

        // ── 3 : Compteurs de ticks ────────────────────────────────────────────
        for (&vmid, &avg) in &vm_avg {
            let profile  = &profiles[&vmid];
            let vcpus    = *vcpus_m.get(&vmid).unwrap_or(&profile.min);
            let has_loan = loans.contains_key(&vmid);
            let state    = vm_states.get_mut(&vmid).unwrap();

            if avg >= config.seuil_detresse && vcpus < profile.max {
                // En détresse et peut encore recevoir
                state.distress_counter += 1;
                state.low_counter       = 0;
            } else if avg <= config.seuil_donneuse && (has_loan || vcpus > profile.min) {
                // En repos — et soit a un prêt à rembourser, soit au-dessus du min
                state.low_counter      += 1;
                state.distress_counter  = 0;
            } else {
                // Saturée ou normal — reset les deux
                state.distress_counter = 0;
                state.low_counter      = 0;
            }
        }

        // ── Affichage état — une fois par cycle ───────────────────────────────
        log_etat(&vm_avg, &profiles, &vcpus_m, &vm_states, &loans,
                 config.seuil_detresse, config.seuil_donneuse);

        // ── 4 : Remboursements ────────────────────────────────────────────────
        // Rembourser 1 vCPU par cycle si en repos depuis N cycles
        // S'arrêter quand vcpus == vcpus_initial (état avant le prêt)
        for (&emp_id, loan) in &loans.clone() {
            if loan.nb_empruntes == 0 { loans.remove(&emp_id); continue; }

            let low = vm_states.get(&emp_id).map(|s| s.low_counter).unwrap_or(0) as usize;
            if low < config.duree_avant_action { continue; }

            let vcpus_emp = *vcpus_m.get(&emp_id).unwrap_or(&0);
            let vcpus_cible = loan.vcpus_initial; // revenir à l'état initial, pas au min

            if vcpus_emp <= vcpus_cible {
                // Déjà à l'état initial, solder le prêt
                loans.remove(&emp_id);
                logger::log_message(&format!(
                    "ℹ️  VM {} revenue à son état initial ({} vCPUs) — prêt soldé", emp_id, vcpus_emp
                ));
                continue;
            }

            let new_emp = vcpus_emp - 1;
            let pret_id = loan.preteur_id;

            if pret_id == 0 {
                // Prêt hôte : libérer sans rendre
                if set_vm_vcpus(emp_id, new_emp).is_some() {
                    vcpus_m.insert(emp_id, new_emp);
                    if let Some(l) = loans.get_mut(&emp_id) {
                        l.nb_empruntes -= 1;
                        if l.nb_empruntes == 0 { loans.remove(&emp_id); }
                    }
                    vm_states.get_mut(&emp_id).unwrap().low_counter = 0;
                    logger::log_message(&format!(
                        "↩️  RETOUR HÔTE : VM {} {} → {} vCPUs | reste: {}",
                        emp_id, vcpus_emp, new_emp,
                        loans.get(&emp_id).map(|l| l.nb_empruntes).unwrap_or(0)
                    ));
                } else {
                    logger::log_message(&format!("❌ Échec retour hôte VM {}", emp_id));
                }
            } else {
                // Prêt VM→VM : rendre à la prêteuse
                let vcpus_pret   = *vcpus_m.get(&pret_id).unwrap_or(&0);
                let profile_pret = match profiles.get(&pret_id) {
                    Some(p) => p,
                    None    => {
                        // Prêteuse absente — libérer quand même
                        if set_vm_vcpus(emp_id, new_emp).is_some() {
                            vcpus_m.insert(emp_id, new_emp);
                            if let Some(l) = loans.get_mut(&emp_id) {
                                l.nb_empruntes -= 1;
                                if l.nb_empruntes == 0 { loans.remove(&emp_id); }
                            }
                            vm_states.get_mut(&emp_id).unwrap().low_counter = 0;
                            logger::log_message(&format!(
                                "↩️  RETOUR (VM {} absente) : VM {} {} → {} vCPUs",
                                pret_id, emp_id, vcpus_emp, new_emp
                            ));
                        }
                        continue;
                    }
                };

                let new_pret         = vcpus_pret + 1;
                let rendre_a_preteur = new_pret <= profile_pret.max;

                let ok_emp  = set_vm_vcpus(emp_id, new_emp).is_some();
                let ok_pret = if rendre_a_preteur {
                    set_vm_vcpus(pret_id, new_pret).is_some()
                } else { true };

                if ok_emp && ok_pret {
                    vcpus_m.insert(emp_id, new_emp);
                    if rendre_a_preteur { vcpus_m.insert(pret_id, new_pret); }
                    if let Some(l) = loans.get_mut(&emp_id) {
                        l.nb_empruntes -= 1;
                        if l.nb_empruntes == 0 { loans.remove(&emp_id); }
                    }
                    vm_states.get_mut(&emp_id).unwrap().low_counter = 0;
                    let reste = loans.get(&emp_id).map(|l| l.nb_empruntes).unwrap_or(0);
                    if rendre_a_preteur {
                        logger::log_message(&format!(
                            "↩️  RETOUR OK : VM {} ({} → {} vCPUs) → VM {} ({} → {} vCPUs) | reste: {}",
                            emp_id, vcpus_emp, new_emp, pret_id, vcpus_pret, new_pret, reste
                        ));
                    } else {
                        logger::log_message(&format!(
                            "↩️  RETOUR OK : VM {} ({} → {} vCPUs) — VM {} au max | reste: {}",
                            emp_id, vcpus_emp, new_emp, pret_id, reste
                        ));
                    }
                } else {
                    if ok_emp && !ok_pret { let _ = set_vm_vcpus(emp_id, vcpus_emp); }
                    logger::log_message(&format!("❌ Échec retour VM {} → VM {}", emp_id, pret_id));
                }
            }
        }

        // ── 5 : Prêts — multi-détresse triée par urgence ─────────────────────
        let mut distress: Vec<(u32, f64)> = vm_avg.iter()
            .filter(|(&vmid, &avg)| {
                let p     = &profiles[&vmid];
                let vcpus = *vcpus_m.get(&vmid).unwrap_or(&p.min);
                let ticks = vm_states[&vmid].distress_counter as usize;
                // Contrainte hardware : vcpus doit rester <= cores configurés
                let cores_max = get_vm_cores_max(vmid);
                avg >= config.seuil_detresse
                    && vcpus < p.max
                    && vcpus < cores_max   // ← vérification hardware
                    && ticks >= config.duree_avant_action
            })
            .map(|(&id, &avg)| (id, avg))
            .collect();
        distress.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

        if distress.len() > 1 {
            logger::log_message(&format!(
                "⚠️  {} VMs en détresse simultanée — traitement par urgence", distress.len()
            ));
        }

        for (dist_id, dist_avg) in &distress {
            let profile_d  = &profiles[dist_id];
            let vcpus_d    = *vcpus_m.get(dist_id).unwrap();
            let cores_max  = get_vm_cores_max(*dist_id);
            let new_dist   = vcpus_d + 1;

            if vcpus_d >= profile_d.max || vcpus_d >= cores_max {
                logger::log_message(&format!(
                    "⚠️  VM {} au plafond ({}/{} vCPUs) — surcharge acceptée",
                    dist_id, vcpus_d, profile_d.max.min(cores_max)
                ));
                continue;
            }

            // Chercher donneur : repos N cycles, pas emprunteur, usage le plus bas
            let donor = vcpus_m.iter()
                .filter(|(&id, &vcpus)| {
                    if id == *dist_id { return false; }
                    let p      = match profiles.get(&id) { Some(p) => p, None => return false };
                    let avg    = vm_avg.get(&id).copied().unwrap_or(1.0);
                    let ticks  = vm_states.get(&id).map(|s| s.low_counter as usize).unwrap_or(0);
                    let loaned = loans.get(&id).map(|l| l.nb_empruntes).unwrap_or(0);
                    // Ne pas emprunter à une VM qui a elle-même emprunté
                    vcpus > p.min
                        && avg <= config.seuil_donneuse
                        && ticks >= config.duree_avant_action
                        && loaned == 0
                })
                .min_by(|a, b| {
                    vm_avg.get(a.0).copied().unwrap_or(1.0)
                        .partial_cmp(&vm_avg.get(b.0).copied().unwrap_or(1.0)).unwrap()
                })
                .map(|(&id, _)| id);

            if let Some(don_id) = donor {
                let vcpus_don = *vcpus_m.get(&don_id).unwrap();
                let don_avg   = vm_avg.get(&don_id).copied().unwrap_or(0.0);

                logger::log_message(&format!(
                    "🔄 PRÊT : VM {} ({:.0}%, {} vCPUs) → VM {} ({:.0}%, {} vCPUs)",
                    don_id, don_avg * 100.0, vcpus_don, dist_id, dist_avg * 100.0, vcpus_d
                ));

                let ok_don  = set_vm_vcpus(don_id, vcpus_don - 1).is_some();
                let ok_dist = set_vm_vcpus(*dist_id, new_dist).is_some();

                if ok_don && ok_dist {
                    vcpus_m.insert(don_id, vcpus_don - 1);
                    vcpus_m.insert(*dist_id, new_dist);

                    // Mémoriser l'état initial seulement au premier prêt
                    let vcpus_initial = loans.get(dist_id)
                        .map(|l| l.vcpus_initial)
                        .unwrap_or(vcpus_d); // état avant CE prêt

                    let loan = loans.entry(*dist_id).or_insert(Loan {
                        preteur_id:    don_id,
                        nb_empruntes:  0,
                        vcpus_initial,
                    });
                    loan.nb_empruntes += 1;

                    vm_states.get_mut(&don_id).unwrap().low_counter       = 0;
                    vm_states.get_mut(dist_id).unwrap().distress_counter   = 0;

                    logger::log_message(&format!(
                        "✅ PRÊT OK : VM {} → {} vCPUs | VM {} → {} vCPUs | VM {} doit {} à VM {} (initial: {})",
                        don_id, vcpus_don - 1, dist_id, new_dist,
                        dist_id, loans[dist_id].nb_empruntes, don_id,
                        loans[dist_id].vcpus_initial
                    ));
                } else {
                    if ok_don && !ok_dist { let _ = set_vm_vcpus(don_id, vcpus_don); }
                    logger::log_message(&format!("❌ Échec prêt VM {} → VM {}", don_id, dist_id));
                }

            } else {
                // Pas de donneur VM → overcommit hôte si activé
                let host   = get_host_cpus();
                let total: u32 = vcpus_m.values().sum();
                let max_hw = (host as f64 * config.cpu_overcommit_ratio) as u32;

                // Vérification hardware
                if new_dist > cores_max {
                    logger::log_message(&format!(
                        "⚠️  VM {} : cores={} insuffisant pour {} vCPUs — ajustez cores dans Proxmox",
                        dist_id, cores_max, new_dist
                    ));
                    continue;
                }

                if config.cpu_overcommit_ratio > 1.0 && total < max_hw {
                    logger::log_message(&format!(
                        "🔄 PRÊT HÔTE : VM {} ({:.0}%) {} → {} vCPUs [{}/{}]",
                        dist_id, dist_avg * 100.0, vcpus_d, new_dist, total + 1, max_hw
                    ));
                    if set_vm_vcpus(*dist_id, new_dist).is_some() {
                        vcpus_m.insert(*dist_id, new_dist);
                        let vcpus_initial = loans.get(dist_id)
                            .map(|l| l.vcpus_initial)
                            .unwrap_or(vcpus_d);
                        let loan = loans.entry(*dist_id).or_insert(Loan {
                            preteur_id: 0, nb_empruntes: 0, vcpus_initial,
                        });
                        loan.nb_empruntes += 1;
                        vm_states.get_mut(dist_id).unwrap().distress_counter = 0;
                        logger::log_message(&format!(
                            "✅ PRÊT HÔTE OK : VM {} → {} vCPUs | doit rendre {} à l'hôte",
                            dist_id, new_dist, loans[dist_id].nb_empruntes
                        ));
                    } else {
                        logger::log_message(&format!("❌ Échec prêt hôte VM {}", dist_id));
                    }
                } else {
                    logger::log_message(&format!(
                        "⚠️  VM {} en détresse ({:.0}%) — aucun donneur disponible, surcharge acceptée",
                        dist_id, dist_avg * 100.0
                    ));
                }
            }
        }

        thread::sleep(Duration::from_secs(config.check_interval));
    }
}

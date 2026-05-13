use crate::config::{AppConfig, Profile};
use crate::logger;
use crate::proxmox::*;
use crate::vm_recognizer::detect_profile;
use crate::signals::{send_migrate_vm, send_lighten_node, Urgency};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// ─── État par VM ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct VMState {
    history:                VecDeque<f64>,
    distress_counter:       u32,
    low_counter:            u32,
    migration_requested:    bool,
    last_migration_attempt: u64,
}

impl VMState {
    fn new(cap: usize) -> Self {
        VMState {
            history:                VecDeque::with_capacity(cap),
            distress_counter:       0,
            low_counter:            0,
            migration_requested:    false,
            last_migration_attempt: 0,
        }
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

#[derive(Debug, Clone)]
struct Loan {
    preteur_id:    u32,  // 0 = hôte
    nb_empruntes:  u32,  // vCPUs empruntés
    vcpus_initial: u32,  // état avant le 1er prêt → cible du retour
}

// ─── Mesure parallèle ─────────────────────────────────────────────────────────

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

fn log_sep() {
    logger::log_message("─────────────────────────────────────────────────");
}

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
        let avg    = vm_avg[&vmid];
        let p      = &profiles[&vmid];
        let vcpus  = *vcpus_map.get(&vmid).unwrap_or(&p.min);
        let state  = &states[&vmid];
        let loan   = loans.get(&vmid);
        let nb_emp = loan.map(|l| l.nb_empruntes).unwrap_or(0);

        let statut = if avg >= sd && vcpus >= p.max { "🔴 SATURÉE " }
                     else if avg >= sd              { "🔴 DÉTRESSE" }
                     else if nb_emp > 0             { "🟠 REMB.   " }
                     else if avg <= sr              { "🟢 REPOS   " }
                     else                           { "🟡 NORMAL  " };

        let pret_str = if nb_emp > 0 {
            let init = loan.map(|l| l.vcpus_initial).unwrap_or(vcpus);
            let pid  = loan.map(|l| l.preteur_id).unwrap_or(0);
            let dest = if pid == 0 { "l'hôte".to_string() } else { format!("VM {}", pid) };
            format!(" [doit rendre {} à {} | cible: {} vCPUs]", nb_emp, dest, init)
        } else { String::new() };

        let mig_str = if state.migration_requested { " [migration demandée]" } else { "" };

        let tick_str = if state.distress_counter > 0 {
            format!("détresse {}c", state.distress_counter)
        } else if state.low_counter > 0 {
            format!("repos {}c", state.low_counter)
        } else { "stable".to_string() };

        logger::log_message(&format!(
            "{} VM {:>3} | CPU={:>5.1}% | vCPUs={} [min={} max={}]{}{} | {}",
            statut, vmid, avg * 100.0, vcpus, p.min, p.max, pret_str, mig_str, tick_str
        ));
    }
    log_sep();
}

// ─── Retour anticipé des prêts avant migration ────────────────────────────────
// Quand une VM va migrer, elle rend immédiatement tous ses vCPUs empruntés
// et revient à son état initial avant de partir
fn retour_anticipe(
    vmid:     u32,
    loans:    &mut HashMap<u32, Loan>,
    vcpus_m:  &mut HashMap<u32, u32>,
    profiles: &HashMap<u32, Profile>,
) {
    if let Some(loan) = loans.remove(&vmid) {
        let vcpus_emp = *vcpus_m.get(&vmid).unwrap_or(&0);

        // 1. Remettre la VM à son état initial
        if vcpus_emp > loan.vcpus_initial {
            if set_vm_vcpus(vmid, loan.vcpus_initial).is_some() {
                vcpus_m.insert(vmid, loan.vcpus_initial);
                logger::log_message(&format!(
                    "↩️  RETOUR ANTICIPÉ : VM {} remise à {} vCPUs (état initial) avant migration",
                    vmid, loan.vcpus_initial
                ));
            }
        }

        // 2. Rendre les vCPUs à la prêteuse si c'est une VM (pas l'hôte)
        if loan.preteur_id != 0 {
            let vcpus_pret = *vcpus_m.get(&loan.preteur_id).unwrap_or(&0);
            let max_pret   = profiles.get(&loan.preteur_id).map(|p| p.max).unwrap_or(u32::MAX);
            let new_pret   = vcpus_pret + loan.nb_empruntes;
            if new_pret <= max_pret {
                if set_vm_vcpus(loan.preteur_id, new_pret).is_some() {
                    vcpus_m.insert(loan.preteur_id, new_pret);
                    logger::log_message(&format!(
                        "↩️  RETOUR ANTICIPÉ : {} vCPU(s) rendus à VM {} avant migration de VM {}",
                        loan.nb_empruntes, loan.preteur_id, vmid
                    ));
                }
            }
        }
    }
}

// ─── Point d'entrée ───────────────────────────────────────────────────────────

pub fn run(config: AppConfig) {
    logger::log_message("╔══════════════════════════════════════════════════╗");
    logger::log_message("║         Agent vCPU Balancer — Démarrage          ║");
    logger::log_message("╚══════════════════════════════════════════════════╝");
    logger::log_message(&format!(
        "Détresse: >{:.0}% | Repos: <{:.0}% | Action après: {}c | Intervalle: {}s | Migration après: {}c",
        config.seuil_detresse * 100.0, config.seuil_donneuse * 100.0,
        config.duree_avant_action, config.check_interval,
        config.distress_before_migration,
    ));

    let mut vm_states: HashMap<u32, VMState> = HashMap::new();
    let mut loans:     HashMap<u32, Loan>    = HashMap::new();

    loop {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // ── 0 : Lister VMs éligibles ─────────────────────────────────────────
        let all = get_all_vms();
        if all.is_empty() {
            logger::log_message("⚠️  Aucune VM, retry dans 5s...");
            thread::sleep(Duration::from_secs(5));
            continue;
        }

        // Détecter les VMs qui ont disparu du nœud (migrées ou éteintes)
        // → rendre leurs prêts immédiatement
        let vmids_presents: std::collections::HashSet<u32> = all.iter().cloned().collect();
        let vmids_avec_pret: Vec<u32> = loans.keys().cloned().collect();
        for vmid in vmids_avec_pret {
            if !vmids_presents.contains(&vmid) {
                logger::log_message(&format!(
                    "🔍 VM {} absente du nœud (migrée/éteinte) — retour anticipé des prêts", vmid
                ));
                retour_anticipe(vmid, &mut loans, &mut HashMap::new(), &HashMap::new());
                vm_states.remove(&vmid);
            }
        }

        let mut profiles: HashMap<u32, Profile> = HashMap::new();
        let mut actifs:   Vec<u32>              = Vec::new();
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
        let metriques = mesurer_toutes_vms(&actifs, &profiles);

        // ── 2 : Historique et moyennes ────────────────────────────────────────
        let mut vm_avg:  HashMap<u32, f64> = HashMap::new();
        let mut vcpus_m: HashMap<u32, u32> = HashMap::new();

        for m in &metriques {
            if profiles.get(&m.vmid).is_none() { continue; }
            vcpus_m.insert(m.vmid, m.vcpus);
            let s = vm_states.entry(m.vmid)
                .or_insert_with(|| VMState::new(config.window_seconds));
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
        let seuil_remboursement = config.seuil_donneuse + 0.10;

        for (&vmid, &avg) in &vm_avg {
            let profile  = &profiles[&vmid];
            let vcpus    = *vcpus_m.get(&vmid).unwrap_or(&profile.min);
            let has_loan = loans.contains_key(&vmid);
            let state    = vm_states.get_mut(&vmid).unwrap();

            if avg < 0.20 {
                // Très bas → forcer le compteur repos au max pour remboursement immédiat
                state.low_counter      = config.duree_avant_action as u32;
                state.distress_counter = 0;
            } else if avg >= config.seuil_detresse && vcpus < profile.max {
                state.distress_counter += 1;
                state.low_counter       = 0;
            } else if avg <= seuil_remboursement && (has_loan || vcpus > profile.min) {
                state.low_counter      += 1;
                state.distress_counter  = 0;
            } else {
                // Zone normale → reset progressif
                state.distress_counter  = 0;
                state.migration_requested = false;
                if state.low_counter > 0 { state.low_counter -= 1; }
            }
        }

        // ── Affichage état ────────────────────────────────────────────────────
        log_etat(&vm_avg, &profiles, &vcpus_m, &vm_states, &loans,
                 config.seuil_detresse, config.seuil_donneuse);

        // ── 4 : Remboursements ────────────────────────────────────────────────
        let loan_ids: Vec<u32> = loans.keys().cloned().collect();
        for emp_id in loan_ids {
            let loan = match loans.get(&emp_id) { Some(l) => l.clone(), None => continue };
            if loan.nb_empruntes == 0 { loans.remove(&emp_id); continue; }

            let low = vm_states.get(&emp_id).map(|s| s.low_counter).unwrap_or(0) as usize;
            if low < config.duree_avant_action { continue; }

            let vcpus_emp = *vcpus_m.get(&emp_id).unwrap_or(&0);
            if vcpus_emp <= loan.vcpus_initial {
                loans.remove(&emp_id);
                logger::log_message(&format!(
                    "✅ VM {} revenue à son état initial ({} vCPUs)", emp_id, vcpus_emp
                ));
                continue;
            }

            // Rembourser 1 vCPU
            let new_emp = vcpus_emp - 1;
            let pret_id = loan.preteur_id;

            let ok_emp = set_vm_vcpus(emp_id, new_emp).is_some();

            // Rendre à la prêteuse
            let ok_pret = if pret_id == 0 {
                true // hôte : libérer sans rendre
            } else {
                let vcpus_pret = *vcpus_m.get(&pret_id).unwrap_or(&0);
                let max_pret   = profiles.get(&pret_id).map(|p| p.max).unwrap_or(u32::MAX);
                let new_pret   = vcpus_pret + 1;
                if new_pret <= max_pret {
                    let ok = set_vm_vcpus(pret_id, new_pret).is_some();
                    if ok { vcpus_m.insert(pret_id, new_pret); }
                    ok
                } else {
                    true // prêteuse au max, libérer sans lui rendre
                }
            };

            if ok_emp && ok_pret {
                vcpus_m.insert(emp_id, new_emp);
                if let Some(l) = loans.get_mut(&emp_id) {
                    if l.nb_empruntes > 0 { l.nb_empruntes -= 1; }
                    if l.nb_empruntes == 0 { loans.remove(&emp_id); }
                }
                vm_states.get_mut(&emp_id).unwrap().low_counter = 0;
                let reste = loans.get(&emp_id).map(|l| l.nb_empruntes).unwrap_or(0);
                if pret_id == 0 {
                    logger::log_message(&format!(
                        "↩️  RETOUR HÔTE : VM {} {} → {} vCPUs | reste: {}",
                        emp_id, vcpus_emp, new_emp, reste
                    ));
                } else {
                    logger::log_message(&format!(
                        "↩️  RETOUR OK : VM {} {} → {} vCPUs → VM {} | reste: {}",
                        emp_id, vcpus_emp, new_emp, pret_id, reste
                    ));
                }
            } else {
                // Rollback si ok_emp mais pas ok_pret
                if ok_emp && !ok_pret {
                    let _ = set_vm_vcpus(emp_id, vcpus_emp);
                }
                logger::log_message(&format!("❌ Échec retour VM {}", emp_id));
            }
        }

        // ── 5 : Prêts ─────────────────────────────────────────────────────────
        let mut distress: Vec<(u32, f64)> = vm_avg.iter()
            .filter(|(&vmid, &avg)| {
                let p         = &profiles[&vmid];
                let vcpus     = *vcpus_m.get(&vmid).unwrap_or(&p.min);
                let ticks     = vm_states[&vmid].distress_counter as usize;
                let cores_max = get_vm_cores_max(vmid);
                // Ne pas prêter à une VM dont la migration est déjà demandée
                let mig_req   = vm_states[&vmid].migration_requested;
                avg >= config.seuil_detresse
                    && vcpus < p.max
                    && vcpus < cores_max
                    && ticks >= config.duree_avant_action
                    && !mig_req
            })
            .map(|(&id, &avg)| (id, avg))
            .collect();
        distress.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

        if distress.len() > 1 {
            logger::log_message(&format!(
                "⚠️  {} VMs en détresse simultanée", distress.len()
            ));
        }

        for (dist_id, dist_avg) in &distress {
            let profile_d = &profiles[dist_id];
            let vcpus_d   = *vcpus_m.get(dist_id).unwrap();
            let cores_max = get_vm_cores_max(*dist_id);
            let new_dist  = vcpus_d + 1;

            if vcpus_d >= profile_d.max || vcpus_d >= cores_max {
                logger::log_message(&format!(
                    "⚠️  VM {} au plafond ({} vCPUs) — surcharge acceptée", dist_id, vcpus_d
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
                    don_id, don_avg * 100.0, vcpus_don,
                    dist_id, dist_avg * 100.0, vcpus_d
                ));

                let ok_don  = set_vm_vcpus(don_id, vcpus_don - 1).is_some();
                let ok_dist = set_vm_vcpus(*dist_id, new_dist).is_some();

                if ok_don && ok_dist {
                    vcpus_m.insert(don_id, vcpus_don - 1);
                    vcpus_m.insert(*dist_id, new_dist);

                    let vcpus_initial = loans.get(dist_id)
                        .map(|l| l.vcpus_initial)
                        .unwrap_or(vcpus_d);

                    let loan = loans.entry(*dist_id).or_insert(Loan {
                        preteur_id: don_id, nb_empruntes: 0, vcpus_initial,
                    });
                    loan.nb_empruntes += 1;

                    vm_states.get_mut(&don_id).unwrap().low_counter      = 0;
                    vm_states.get_mut(dist_id).unwrap().distress_counter  = 0;

                    logger::log_message(&format!(
                        "✅ PRÊT OK : VM {} → {} vCPUs | VM {} → {} vCPUs | cible retour: {} vCPUs",
                        don_id, vcpus_don - 1, dist_id, new_dist,
                        loans[dist_id].vcpus_initial
                    ));
                } else {
                    if ok_don && !ok_dist { let _ = set_vm_vcpus(don_id, vcpus_don); }
                    logger::log_message(&format!("❌ Échec prêt VM {} → VM {}", don_id, dist_id));
                }

            } else {
                // Pas de donneur VM → overcommit hôte
                let host   = get_host_cpus();
                let total: u32 = vcpus_m.values().sum();
                let max_hw = (host as f64 * config.cpu_overcommit_ratio) as u32;

                if new_dist > cores_max {
                    logger::log_message(&format!(
                        "⚠️  VM {} : cores={} insuffisant — ajustez cores dans Proxmox",
                        dist_id, cores_max
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
                            .map(|l| l.vcpus_initial).unwrap_or(vcpus_d);
                        let loan = loans.entry(*dist_id).or_insert(Loan {
                            preteur_id: 0, nb_empruntes: 0, vcpus_initial,
                        });
                        loan.nb_empruntes += 1;
                        vm_states.get_mut(dist_id).unwrap().distress_counter = 0;
                        logger::log_message(&format!(
                            "✅ PRÊT HÔTE OK : VM {} → {} vCPUs | cible retour: {} vCPUs",
                            dist_id, new_dist, loans[dist_id].vcpus_initial
                        ));
                    } else {
                        logger::log_message(&format!("❌ Échec prêt hôte VM {}", dist_id));
                    }
                } else {
                    logger::log_message(&format!(
                        "⚠️  VM {} en détresse ({:.0}%) — aucun donneur, surcharge acceptée",
                        dist_id, dist_avg * 100.0
                    ));
                }
            }
        }

        // ── 6 : Signaux de migration ──────────────────────────────────────────
        // Déclenché quand une VM est en détresse persistante sans solution locale
        // La VM rend ses prêts AVANT de partir, la prêteuse récupère ses vCPUs

        let mut nb_migration_demandee = 0u32;

        for (&dist_id, &dist_avg) in &vm_avg {
            let state = match vm_states.get_mut(&dist_id) {
                Some(s) => s,
                None    => continue,
            };

            let distress_ticks = state.distress_counter as usize;

            // Conditions pour demander une migration :
            // 1. En détresse depuis assez longtemps
            // 2. Pas déjà demandée
            // 3. Cooldown respecté
            if distress_ticks >= config.distress_before_migration
                && !state.migration_requested
                && now.saturating_sub(state.last_migration_attempt) > config.migration_cooldown_seconds
            {
                // Retour anticipé des prêts AVANT d'émettre le signal
                // La VM part proprement, la prêteuse récupère ses vCPUs immédiatement
                retour_anticipe(dist_id, &mut loans, &mut vcpus_m, &profiles);

                let reason = format!(
                    "vm_{}_cpu_{:.0}pct_no_local_donor_{}cycles",
                    dist_id, dist_avg * 100.0, distress_ticks
                );

                if send_migrate_vm(dist_id, reason, Urgency::High) {
                    state.migration_requested    = true;
                    state.last_migration_attempt = now;
                    nb_migration_demandee += 1;

                    logger::log_message(&format!(
                        "🚨 MIGRATION demandée pour VM {} ({:.0}% depuis {}c) — prêts rendus",
                        dist_id, dist_avg * 100.0, distress_ticks
                    ));
                }
            }
        }

        // Si plusieurs VMs en détresse persistante → LIGHTEN_NODE en plus
        let nb_detresse_persistante = vm_avg.iter()
            .filter(|(&vmid, &avg)| {
                avg >= config.seuil_detresse
                && vm_states.get(&vmid)
                    .map(|s| s.distress_counter as usize >= config.distress_before_migration)
                    .unwrap_or(false)
            })
            .count();

        if nb_detresse_persistante > 1 {
            let reason = format!(
                "cpu_contention_{}_vms_persistant_distress", nb_detresse_persistante
            );
            if send_lighten_node(reason, Urgency::High) {
                logger::log_message(&format!(
                    "🚨 LIGHTEN_NODE émis — {} VMs en détresse persistante", nb_detresse_persistante
                ));
            }
        }

        thread::sleep(Duration::from_secs(config.check_interval));
    }
}

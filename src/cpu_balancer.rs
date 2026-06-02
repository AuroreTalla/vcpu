use crate::config::{AppConfig, Profile};
use crate::logger;
use crate::proxmox::*;
use crate::vm_recognizer::detect_profile;
use crate::signals::{send_migrate_vm, send_lighten_node, Urgency};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// ─── Segment de prêt ──────────────────────────────────────────────────────────
#[derive(Debug, Clone)]
struct LoanSegment {
    preteur_id:   u32, // 0 = hôte
    nb_empruntes: u32,
}

// ─── Prêt actif (multi-créanciers, LIFO) ─────────────────────────────────────
#[derive(Debug, Clone)]
struct Loan {
    vcpus_initial: u32,
    segments:      Vec<LoanSegment>,
}

impl Loan {
    fn new(vcpus_initial: u32) -> Self {
        Loan { vcpus_initial, segments: Vec::new() }
    }

    fn total_empruntes(&self) -> u32 {
        self.segments.iter().map(|s| s.nb_empruntes).sum()
    }

    fn ajouter(&mut self, preteur_id: u32, nb: u32) {
        if let Some(seg) = self.segments.iter_mut().find(|s| s.preteur_id == preteur_id) {
            seg.nb_empruntes += nb;
        } else {
            self.segments.push(LoanSegment { preteur_id, nb_empruntes: nb });
        }
    }

    /// Retire 1 vCPU du segment le plus récent (LIFO), retourne preteur_id.
    fn retirer_un(&mut self) -> Option<u32> {
        while let Some(seg) = self.segments.last_mut() {
            if seg.nb_empruntes == 0 { self.segments.pop(); continue; }
            let pid = seg.preteur_id;
            seg.nb_empruntes -= 1;
            if seg.nb_empruntes == 0 { self.segments.pop(); }
            return Some(pid);
        }
        None
    }
}

// ─── Statut de migration ──────────────────────────────────────────────────────
#[derive(Debug, Clone, PartialEq)]
enum MigrationStatus {
    None,
    Requested { since: u64 },
    Confirmed,
}

// ─── État par VM ──────────────────────────────────────────────────────────────
#[derive(Debug, Clone)]
struct VMState {
    /// Fenêtre glissante des mesures brutes (taille = window_size).
    history:          VecDeque<f64>,
    distress_counter: u32,
    low_counter:      u32,
    migration:        MigrationStatus,
    /// Cycle où les vCPUs ont été modifiés — on ignore la mesure suivante.
    vcpus_changed_at: u64,
    /// Timestamp (ms) de la dernière mesure acceptée — rejeter les mesures
    /// antérieures au début du cycle courant.
    last_measure_ts:  u64,
}

impl VMState {
    fn new(window_size: usize) -> Self {
        VMState {
            history:          VecDeque::with_capacity(window_size),
            distress_counter: 0,
            low_counter:      0,
            migration:        MigrationStatus::None,
            vcpus_changed_at: 0,
            last_measure_ts:  0,
        }
    }

    fn push(&mut self, v: f64, ts_ms: u64, window_size: usize, cycle_start_ms: u64) {
        // FIX: rejeter les mesures dont le timestamp est antérieur au début du cycle.
        // Protège contre les mesures périmées retournées par des threads lents.
        if ts_ms < cycle_start_ms {
            logger::log_message(&format!(
                "⚠️  Mesure rejetée (ts={}ms < cycle_start={}ms)", ts_ms, cycle_start_ms
            ));
            return;
        }
        self.last_measure_ts = ts_ms;
        self.history.push_back(v);
        if self.history.len() > window_size { self.history.pop_front(); }
    }

    fn avg(&self) -> Option<f64> {
        if self.history.is_empty() { return None; }
        Some(self.history.iter().sum::<f64>() / self.history.len() as f64)
    }

    fn ready(&self, min_samples: usize) -> bool {
        self.history.len() >= min_samples
    }

    fn migration_requested(&self) -> bool {
        matches!(self.migration, MigrationStatus::Requested { .. })
    }
}

// ─── Mesure parallèle ─────────────────────────────────────────────────────────
#[derive(Debug, Clone)]
struct VMMetric {
    vmid:     u32,
    usage:    f64,  // [0.0, 1.0]
    ts_ms:    u64,  // FIX: timestamp de la mesure pour rejet si périmée
    vcpus:    u32,
}

fn mesurer_toutes_vms(
    vmids:          &[u32],
    profiles:       &HashMap<u32, Profile>,
    vcpus_known:    &HashMap<u32, u32>,
    cycle:          u64,
    vm_states:      &HashMap<u32, VMState>,
    cycle_start_ms: u64,
) -> Vec<VMMetric> {
    let res: Arc<Mutex<Vec<VMMetric>>> = Arc::new(Mutex::new(Vec::new()));
    let mut handles = Vec::new();

    for &vmid in vmids {
        let profile     = match profiles.get(&vmid) { Some(p) => p.clone(), None => continue };
        let vcpus_local = vcpus_known.get(&vmid).copied();

        // FIX: sauter la mesure pendant le cooldown post-changement de vCPUs.
        // On utilise vcpu_change_cooldown_cycles depuis config — ici passé
        // implicitement via la comparaison sur vcpus_changed_at.
        let skip = vm_states
            .get(&vmid)
            .map(|s| s.vcpus_changed_at >= cycle.saturating_sub(1))
            .unwrap_or(false);

        if skip { continue; } // ne pas pousser dans l'historique du tout

        let r = Arc::clone(&res);
        handles.push(thread::spawn(move || {
            // FIX: vCPUs depuis map locale — get_current_vcpus() seulement
            // à la première apparition d'une VM (vcpus_local = None).
            let vcpus = vcpus_local.unwrap_or_else(|| {
                get_current_vcpus(vmid).unwrap_or(profile.min)
            });

            // FIX: get_vm_cpu_usage() retourne (f64, u64) — on propage le timestamp.
            if let Some((usage, ts_ms)) = get_vm_cpu_usage(vmid) {
                r.lock().unwrap().push(VMMetric {
                    vmid,
                    usage: usage.clamp(0.0, 1.0),
                    ts_ms,
                    vcpus,
                });
            }
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
    sd: f64,
    sr: f64,
) {
    let mut ids: Vec<u32> = vm_avg.keys().cloned().collect();
    ids.sort();

    for vmid in ids {
        let avg       = vm_avg[&vmid];
        let p         = &profiles[&vmid];
        let vcpus     = *vcpus_map.get(&vmid).unwrap_or(&p.min);
        let state     = &states[&vmid];
        let loan      = loans.get(&vmid);
        let total_emp = loan.map(|l| l.total_empruntes()).unwrap_or(0);

        let statut = if avg >= sd && vcpus >= p.max { "🔴 SATURÉE " }
                     else if avg >= sd              { "🔴 DÉTRESSE" }
                     else if total_emp > 0          { "🟠 REMB.   " }
                     else if avg <= sr              { "🟢 REPOS   " }
                     else                           { "🟡 NORMAL  " };

        let pret_str = if let Some(l) = loan {
            if l.total_empruntes() > 0 {
                let credits: Vec<String> = l.segments.iter().map(|s| {
                    let dest = if s.preteur_id == 0 { "hôte".to_string() }
                               else { format!("VM{}", s.preteur_id) };
                    format!("{}×{}", s.nb_empruntes, dest)
                }).collect();
                format!(" [doit rendre {} ({}) | cible: {} vCPUs]",
                    l.total_empruntes(), credits.join("+"), l.vcpus_initial)
            } else { String::new() }
        } else { String::new() };

        let mig_str = match &state.migration {
            MigrationStatus::Requested { .. } => " [migration demandée]",
            MigrationStatus::Confirmed        => " [migration confirmée]",
            MigrationStatus::None             => "",
        };

        let tick_str = if state.distress_counter > 0 {
            format!("détresse {}c", state.distress_counter)
        } else if state.low_counter > 0 {
            format!("repos {}c", state.low_counter)
        } else {
            "stable".to_string()
        };

        logger::log_message(&format!(
            "{} VM {:>3} | CPU={:>5.1}% | vCPUs={} [min={} max={}]{}{} | {}",
            statut, vmid, avg * 100.0, vcpus, p.min, p.max, pret_str, mig_str, tick_str
        ));
    }
    log_sep();
}

// ─── Retour anticipé multi-créanciers ────────────────────────────────────────
// FIX: vérifie que chaque créancier est encore dans vmids_presents avant retour.
fn retour_anticipe(
    vmid:           u32,
    loans:          &mut HashMap<u32, Loan>,
    vcpus_m:        &mut HashMap<u32, u32>,
    profiles:       &HashMap<u32, Profile>,
    vmids_presents: &HashSet<u32>,
    vm_states:      &mut HashMap<u32, VMState>,
    cycle:          u64,
) {
    let loan = match loans.remove(&vmid) { Some(l) => l, None => return };
    let vcpus_emp = *vcpus_m.get(&vmid).unwrap_or(&0);

    // 1. Remettre l'emprunteuse à son état initial
    if vcpus_emp != loan.vcpus_initial {
        if set_vm_vcpus(vmid, loan.vcpus_initial).is_some() {
            vcpus_m.insert(vmid, loan.vcpus_initial);
            if let Some(s) = vm_states.get_mut(&vmid) { s.vcpus_changed_at = cycle; }
            logger::log_message(&format!(
                "↩️  RETOUR ANTICIPÉ : VM {} → {} vCPUs (état initial)",
                vmid, loan.vcpus_initial
            ));
        }
    }

    // 2. Rembourser chaque créancier — vérification présence
    for seg in &loan.segments {
        if seg.preteur_id == 0 || seg.nb_empruntes == 0 { continue; }

        // FIX: ne rendre que si le créancier est encore présent
        if !vmids_presents.contains(&seg.preteur_id) {
            logger::log_message(&format!(
                "⚠️  Créancier VM {} absent — {} vCPU(s) libérés sans retour",
                seg.preteur_id, seg.nb_empruntes
            ));
            continue;
        }

        let vcpus_pret = *vcpus_m.get(&seg.preteur_id).unwrap_or(&0);
        let max_pret   = profiles.get(&seg.preteur_id).map(|p| p.max).unwrap_or(u32::MAX);
        let new_pret   = vcpus_pret + seg.nb_empruntes;

        if new_pret <= max_pret {
            if set_vm_vcpus(seg.preteur_id, new_pret).is_some() {
                vcpus_m.insert(seg.preteur_id, new_pret);
                if let Some(s) = vm_states.get_mut(&seg.preteur_id) {
                    s.vcpus_changed_at = cycle;
                }
                logger::log_message(&format!(
                    "↩️  RETOUR ANTICIPÉ : {} vCPU(s) rendus à VM {} (migration VM {})",
                    seg.nb_empruntes, seg.preteur_id, vmid
                ));
            }
        } else {
            logger::log_message(&format!(
                "⚠️  VM {} au max ({}) — {} vCPU(s) libérés sans retour",
                seg.preteur_id, max_pret, seg.nb_empruntes
            ));
        }
    }
}

// ─── Point d'entrée ───────────────────────────────────────────────────────────
pub fn run(config: AppConfig) {
    logger::log_message("╔══════════════════════════════════════════════════╗");
    logger::log_message("║         Agent vCPU Balancer — Démarrage          ║");
    logger::log_message("╚══════════════════════════════════════════════════╝");
    logger::log_message(&format!(
        "Détresse: >{:.0}% | Repos: <{:.0}% | Fenêtre: {} | Min-samples: {} | Intervalle: {}s | Migration après: {}c",
        config.seuil_detresse * 100.0,
        config.seuil_donneuse * 100.0,
        config.window_size,
        config.min_samples_before_action,
        config.check_interval,
        config.distress_before_migration,
    ));

    let mut vm_states:   HashMap<u32, VMState> = HashMap::new();
    let mut loans:       HashMap<u32, Loan>    = HashMap::new();
    let mut vcpus_known: HashMap<u32, u32>     = HashMap::new();
    let mut cycle: u64 = 0;

    loop {
        cycle += 1;

        // FIX: timestamp de début de cycle — toute mesure antérieure est rejetée.
        let cycle_start_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let now_secs = cycle_start_ms / 1000;

        // ── 0 : Lister VMs éligibles ─────────────────────────────────────────
        let all = get_all_vms();
        logger::log_message(&format!(
            "🔍 Cycle {} — {} VMs trouvées : {:?}", cycle, all.len(), all
        ));

        if all.is_empty() {
            logger::log_message("⚠️  Aucune VM, retry dans 5s...");
            thread::sleep(Duration::from_secs(5));
            continue;
        }

        let vmids_presents: HashSet<u32> = all.iter().cloned().collect();

        // ── Profils ───────────────────────────────────────────────────────────
        // FIX: detect_profile(vmid, profiles) — plus de paramètre vm_name.
        // Le nom est lu en interne uniquement pour la détection de profil.
        let mut profiles: HashMap<u32, Profile> = HashMap::new();
        let mut actifs:   Vec<u32>              = Vec::new();
        for &vmid in &all {
            if let Some(p) = detect_profile(vmid, &config.profiles) {
                profiles.insert(vmid, p);
                actifs.push(vmid);
            }
        }

        if actifs.is_empty() {
            logger::log_message("⚠️ Aucune VM éligible (profils introuvables)");
            thread::sleep(Duration::from_secs(config.check_interval));
            continue;
        }

        // ── Purger vcpus_known des VMs disparues ──────────────────────────────
        vcpus_known.retain(|id, _| vmids_presents.contains(id));

        // ── Purger vm_states de TOUTES les VMs disparues ─────────────────────
        // FIX: purge AVANT les mesures, pas seulement pour les VMs avec prêt.
        {
            // D'abord traiter les prêts des VMs disparues (retour anticipé)
            let vmids_avec_pret: Vec<u32> = loans.keys().cloned().collect();
            for vmid in vmids_avec_pret {
                if !vmids_presents.contains(&vmid) {
                    logger::log_message(&format!(
                        "🔍 VM {} disparue — retour anticipé des prêts", vmid
                    ));
                    // On passe un profiles vide pour les VMs disparues (pas de profil connu)
                    retour_anticipe(
                        vmid, &mut loans, &mut vcpus_known, &profiles,
                        &vmids_presents, &mut vm_states, cycle,
                    );
                }
            }
            // FIX: purger vm_states de TOUTES les VMs disparues, pas seulement celles avec prêt.
            vm_states.retain(|id, _| vmids_presents.contains(id));
        }

        // ── 1 : Mesures PARALLÈLES ────────────────────────────────────────────
        let metriques = mesurer_toutes_vms(
            &actifs, &profiles, &vcpus_known, cycle, &vm_states, cycle_start_ms,
        );

        // ── 2 : Historique, moyennes, mise à jour vcpus_known ────────────────
        let mut vm_avg:  HashMap<u32, f64> = HashMap::new();
        let mut vcpus_m: HashMap<u32, u32> = HashMap::new();

        for m in &metriques {
            if profiles.get(&m.vmid).is_none() { continue; }

            vcpus_known.insert(m.vmid, m.vcpus);
            vcpus_m.insert(m.vmid, m.vcpus);

            let s = vm_states
                .entry(m.vmid)
                .or_insert_with(|| VMState::new(config.window_size));

            // FIX: push avec timestamp — mesure rejetée si antérieure au cycle.
            s.push(m.usage, m.ts_ms, config.window_size, cycle_start_ms);

            if s.ready(config.min_samples_before_action) {
                if let Some(avg) = s.avg() {
                    vm_avg.insert(m.vmid, avg);
                }
            }
        }

        // Compléter vcpus_m pour les VMs sans mesure ce cycle (cooldown)
        for &vmid in &actifs {
            if !vcpus_m.contains_key(&vmid) {
                if let Some(&v) = vcpus_known.get(&vmid) {
                    vcpus_m.insert(vmid, v);
                }
            }
        }

        if vm_avg.is_empty() {
            logger::log_message(&format!(
                "⏳ Cycle {} — accumulation des mesures ({}/{} min)",
                cycle,
                vm_states.values().map(|s| s.history.len()).max().unwrap_or(0),
                config.min_samples_before_action
            ));
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
                state.low_counter      = config.duree_avant_action as u32;
                state.distress_counter = 0;
            } else if avg >= config.seuil_detresse && vcpus < profile.max {
		state.distress_counter += 1;
                state.low_counter       = 0;
		if let MigrationStatus::Requested { since } = state.migration {
        		if now_secs.saturating_sub(since) > config.migration_cooldown_seconds {
       			     state.migration = MigrationStatus::None;
               		}
    		}
            } else if avg <= seuil_remboursement && (has_loan || vcpus > profile.min) {
                state.low_counter      += 1;
                state.distress_counter  = 0;
            } else {
                state.distress_counter = 0;
                if state.low_counter > 0 { state.low_counter -= 1; }
		if let MigrationStatus::Requested { since } = state.migration {
     		   if now_secs.saturating_sub(since) > config.migration_cooldown_seconds {
            		state.migration = MigrationStatus::None;
            		logger::log_message(&format!(
               		 "🔄 VM {} : migration annulée (revenue à la normale, cooldown expiré)", vmid
            ));
        }
    }
            }
        }

        // ── Affichage ─────────────────────────────────────────────────────────
        log_etat(
            &vm_avg, &profiles, &vcpus_m, &vm_states, &loans,
            config.seuil_detresse, config.seuil_donneuse,
        );

        // ── 4 : Remboursements ────────────────────────────────────────────────
        let loan_ids: Vec<u32> = loans.keys().cloned().collect();
        for emp_id in loan_ids {
            {
                let loan = match loans.get(&emp_id) { Some(l) => l, None => continue };
                if loan.total_empruntes() == 0 {
                    loans.remove(&emp_id);
                    continue;
                }
                let low = vm_states.get(&emp_id)
                    .map(|s| s.low_counter as usize)
                    .unwrap_or(0);
                if low < config.duree_avant_action { continue; }

                let vcpus_emp = *vcpus_m.get(&emp_id).unwrap_or(&0);
                if vcpus_emp <= loan.vcpus_initial {
                    loans.remove(&emp_id);
                    logger::log_message(&format!(
                        "✅ VM {} revenue à son état initial ({} vCPUs)", emp_id, vcpus_emp
                    ));
                    continue;
                }
            }

            let vcpus_emp = *vcpus_m.get(&emp_id).unwrap_or(&0);
            let new_emp   = vcpus_emp - 1;

            let pret_id = loans.get(&emp_id)
                .and_then(|l| l.segments.last())
                .map(|s| s.preteur_id);

            let ok_emp = set_vm_vcpus(emp_id, new_emp).is_some();
            if ok_emp {
                vcpus_m.insert(emp_id, new_emp);
                vcpus_known.insert(emp_id, new_emp);
                if let Some(s) = vm_states.get_mut(&emp_id) {
                    s.vcpus_changed_at = cycle;
                }
            }

            let ok_pret = match pret_id {
                None | Some(0) => true,
                Some(pid) => {
                    // FIX: vérifier que le créancier est encore présent
                    if !vmids_presents.contains(&pid) {
                        logger::log_message(&format!(
                            "⚠️  Créancier VM {} absent — vCPU libéré sans retour", pid
                        ));
                        true
                    } else {
                        let vcpus_pret = *vcpus_m.get(&pid).unwrap_or(&0);
                        let max_pret   = profiles.get(&pid).map(|p| p.max).unwrap_or(u32::MAX);
                        let new_pret   = vcpus_pret + 1;
                        if new_pret <= max_pret {
                            let ok = set_vm_vcpus(pid, new_pret).is_some();
                            if ok {
                                vcpus_m.insert(pid, new_pret);
                                vcpus_known.insert(pid, new_pret);
                                if let Some(s) = vm_states.get_mut(&pid) {
                                    s.vcpus_changed_at = cycle;
                                }
                            }
                            ok
                        } else {
                            true
                        }
                    }
                }
            };

            if ok_emp {
                if let Some(loan) = loans.get_mut(&emp_id) {
                    loan.retirer_un();
                    if loan.total_empruntes() == 0 { loans.remove(&emp_id); }
                }
                if let Some(s) = vm_states.get_mut(&emp_id) { s.low_counter = 0; }
                let reste = loans.get(&emp_id).map(|l| l.total_empruntes()).unwrap_or(0);
                match pret_id {
                    None | Some(0) => logger::log_message(&format!(
                        "↩️  RETOUR HÔTE : VM {} {} → {} vCPUs | reste: {}",
                        emp_id, vcpus_emp, new_emp, reste
                    )),
                    Some(pid) => logger::log_message(&format!(
                        "↩️  RETOUR OK : VM {} {} → {} vCPUs → VM {} | reste: {}",
                        emp_id, vcpus_emp, new_emp, pid, reste
                    )),
                }
                if !ok_pret {
                    logger::log_message(&format!(
                        "⚠️  Retour partiel VM {} : emprunteuse OK mais créancière KO", emp_id
                    ));
                }
            } else {
                logger::log_message(&format!("❌ Échec retour VM {}", emp_id));
            }
        }

        // ── 5 : Prêts ─────────────────────────────────────────────────────────
        let cores_cache: HashMap<u32, u32> = actifs.iter()
    		.map(|&id| (id, get_vm_cores_max(id)))
    		.collect();
        let mut distress: Vec<(u32, f64)> = vm_avg.iter()
            .filter(|(&vmid, &avg)| {
                let p         = &profiles[&vmid];
                let vcpus     = *vcpus_m.get(&vmid).unwrap_or(&p.min);
                let ticks     = vm_states[&vmid].distress_counter as usize;
                let cores_max = cores_cache.get(&vmid).copied().unwrap_or(1);
                let mig_req   = vm_states[&vmid].migration_requested();
                
                let est_emprunteuse = loans.contains_key(&vmid);
                avg >= config.seuil_detresse
                    && vcpus < p.max
                    && vcpus < cores_max
                    && ticks >= config.duree_avant_action
                    && !mig_req
                    && !est_emprunteuse  // FIX: une emprunteuse rembourse d'abord
            })
            .map(|(&id, &avg)| (id, avg))
            .collect();

        // Tiebreak sur vmid pour reproductibilité
        distress.sort_by(|a, b| {
            b.1.partial_cmp(&a.1).unwrap().then_with(|| a.0.cmp(&b.0))
        });

        if distress.len() > 1 {
            logger::log_message(&format!(
                "⚠️  {} VMs en détresse simultanée", distress.len()
            ));
        }

        for (dist_id, dist_avg) in &distress {
            let profile_d = &profiles[dist_id];
            let vcpus_d   = *vcpus_m.get(dist_id).unwrap();
            let cores_max = cores_cache.get(dist_id).copied().unwrap_or(1);
            let new_dist  = vcpus_d + 1;

            if vcpus_d >= profile_d.max || vcpus_d >= cores_max {
                logger::log_message(&format!(
                    "⚠️  VM {} au plafond ({} vCPUs) — aucune action locale", dist_id, vcpus_d
                ));
                continue;
            }

            // Chercher donneur :
            // FIX: !loans.contains_key(&id) → une emprunteuse ne peut pas prêter.
            // FIX: filtrer sur vcpus réels > profile.min (pas vcpus_initial).
            let donor = vcpus_m.iter()
                .filter(|(&id, &vcpus)| {
                    if id == *dist_id { return false; }
                    let p   = match profiles.get(&id) { Some(p) => p, None => return false };
                    let avg = vm_avg.get(&id).copied().unwrap_or(1.0);
                    let ticks = vm_states.get(&id)
                        .map(|s| s.low_counter as usize)
                        .unwrap_or(0);
                    let mig = vm_states.get(&id)
                        .map(|s| s.migration_requested())
                        .unwrap_or(false);
                    // FIX: double condition emprunteuse
                    let est_emprunteuse = loans.contains_key(&id);
                    !est_emprunteuse       // une emprunteuse ne prête pas
                        && !mig            // une VM en migration ne prête pas
                        && vcpus > p.min   // FIX: filtrer sur vCPUs réels
                        && avg <= config.seuil_donneuse
                        && ticks >= config.duree_avant_action
                })
                .min_by(|a, b| {
                    let avg_a = vm_avg.get(a.0).copied().unwrap_or(1.0);
                    let avg_b = vm_avg.get(b.0).copied().unwrap_or(1.0);
                    avg_a.partial_cmp(&avg_b).unwrap().then_with(|| a.0.cmp(b.0))
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
                let ok_dist = if ok_don { set_vm_vcpus(*dist_id, new_dist).is_some() } else { false };

                if ok_don && ok_dist {
                    vcpus_m.insert(don_id, vcpus_don - 1);
                    vcpus_m.insert(*dist_id, new_dist);
                    vcpus_known.insert(don_id, vcpus_don - 1);
                    vcpus_known.insert(*dist_id, new_dist);

                    if let Some(s) = vm_states.get_mut(&don_id) {
                        s.vcpus_changed_at = cycle;
                        s.low_counter      = 0;
                    }
                    if let Some(s) = vm_states.get_mut(dist_id) {
                        s.vcpus_changed_at  = cycle;
                        s.distress_counter  = 0;
                    }

                    // Enregistrer le segment
                    let vcpus_initial = loans.get(dist_id)
                        .map(|l| l.vcpus_initial)
                        .unwrap_or(vcpus_d);
                    let loan = loans.entry(*dist_id)
                        .or_insert_with(|| Loan::new(vcpus_initial));
                    loan.ajouter(don_id, 1);

                    logger::log_message(&format!(
                        "✅ PRÊT OK : VM {} → {} vCPUs | VM {} → {} vCPUs | cible retour: {} vCPUs",
                        don_id, vcpus_don - 1, dist_id, new_dist,
                        vcpus_initial
                    ));
                } else {
                    if ok_don && !ok_dist { let _ = set_vm_vcpus(don_id, vcpus_don); }
                    logger::log_message(&format!(
                        "❌ Échec prêt VM {} → VM {}", don_id, dist_id
                    ));
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
                        vcpus_known.insert(*dist_id, new_dist);

                        if let Some(s) = vm_states.get_mut(dist_id) {
                            s.vcpus_changed_at  = cycle;
                            s.distress_counter  = 0;
                        }

                        let vcpus_initial = loans.get(dist_id)
                            .map(|l| l.vcpus_initial)
                            .unwrap_or(vcpus_d);
                        let loan = loans.entry(*dist_id)
                            .or_insert_with(|| Loan::new(vcpus_initial));
                        loan.ajouter(0, 1); // prêteur = hôte

                        logger::log_message(&format!(
                            "✅ PRÊT HÔTE OK : VM {} → {} vCPUs | cible retour: {} vCPUs",
                            dist_id, new_dist, vcpus_initial
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
        // FIX: distress_counter remis à 0 après émission pour ne pas re-déclencher.
        // FIX: retour anticipé AVANT le signal (emprunteuse rend ses vCPUs avant de partir).
        // FIX: une VM en MigrationStatus::Confirmed n'est plus candidate.

        let candidats_migration: Vec<(u32, f64, usize)> = vm_avg.iter()
            .filter_map(|(&vmid, &avg)| {
                let state = vm_states.get(&vmid)?;
                let ticks = state.distress_counter as usize;
                let cooldown_ok = match &state.migration {
                    MigrationStatus::Requested { since } =>
                        now_secs.saturating_sub(*since) > config.migration_cooldown_seconds,
                    MigrationStatus::None      => true,
                    MigrationStatus::Confirmed => false,
                };
                if ticks >= config.distress_before_migration && cooldown_ok {
                    Some((vmid, avg, ticks))
                } else {
                    None
                }
            })
            .collect();

        let mut nb_migration_demandee = 0u32;

        for (dist_id, dist_avg, distress_ticks) in candidats_migration {
            // FIX: retour anticipé — la VM rend tous ses emprunts AVANT de migrer.
            // Si elle est créancière, les emprunteuses gardent leurs vCPUs
            // (les segments sont nettoyés dans retour_anticipe via vmids_presents).
            retour_anticipe(
                dist_id, &mut loans, &mut vcpus_known, &profiles,
                &vmids_presents, &mut vm_states, cycle,
            );

            let reason = format!(
                "vm_{}_cpu_{:.0}pct_no_local_donor_{}cycles",
                dist_id, dist_avg * 100.0, distress_ticks
            );

            if send_migrate_vm(dist_id, reason, Urgency::High) {
                if let Some(state) = vm_states.get_mut(&dist_id) {
                    // FIX: reset distress_counter après émission
                    state.distress_counter = 0;
                    state.migration        = MigrationStatus::Requested { since: now_secs };
                }
                nb_migration_demandee += 1;

                logger::log_message(&format!(
                    "🚨 MIGRATION demandée VM {} ({:.0}% depuis {}c) — prêts rendus, compteur reset",
                    dist_id, dist_avg * 100.0, distress_ticks
                ));
            }
        }

        // ── Pression nœud ─────────────────────────────────────────────────────
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
                    "🚨 LIGHTEN_NODE émis — {} VMs en détresse persistante",
                    nb_detresse_persistante
                ));
            }
        }

        thread::sleep(Duration::from_secs(config.check_interval));
    }
}

use serde_json::Value;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};
use once_cell::sync::Lazy;

// ── Nom du nœud (calculé une seule fois au démarrage) ────────────────────────
pub static NODE_NAME: Lazy<String> = Lazy::new(|| {
    Command::new("hostname")
        .arg("-s")
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                String::from_utf8(o.stdout).ok().map(|s| s.trim().to_string())
            } else {
                None
            }
        })
        .unwrap_or_else(|| "localhost".to_string())
});

// ── Exécuteurs directs (jamais via shell) ─────────────────────────────────────

fn run_qm(args: &[&str]) -> Option<String> {
    let output = Command::new("qm").args(args).output().ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if !stderr.is_empty() {
            crate::logger::log_message(&format!(
                "⚠️  qm {:?} → {}", args, stderr
            ));
        }
        None
    }
}

fn run_pvesh(args: &[&str]) -> Option<String> {
    let output = Command::new("pvesh").args(args).output().ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

// ── Inventaire des VMs ────────────────────────────────────────────────────────
// Retourne tous les VMIDs en cours d'exécution sur ce nœud,
// en excluant les templates (template=1).
pub fn get_all_vms() -> Vec<u32> {
    let path = format!("/nodes/{}/qemu", *NODE_NAME);
    let out = match run_pvesh(&["get", &path, "--output-format=json"]) {
        Some(o) => o,
        None    => return vec![],
    };
    let vms: Vec<Value> = match serde_json::from_str(&out) {
        Ok(v)  => v,
        Err(_) => return vec![],
    };
    let mut ids: Vec<u32> = vms
        .iter()
        .filter(|v| v["template"].as_u64().unwrap_or(0) == 0)
        .filter(|v| v["status"].as_str().unwrap_or("") == "running")
        .filter_map(|v| v["vmid"].as_u64().map(|id| id as u32))
        .collect();
    ids.sort_unstable();
    ids
}

// ── Config Proxmox d'une VM ───────────────────────────────────────────────────
pub fn get_vm_config(vmid: u32) -> Option<Value> {
    let path = format!("/nodes/{}/qemu/{}/config", *NODE_NAME, vmid);
    let out = run_pvesh(&["get", &path, "--output-format=json"])?;
    serde_json::from_str(&out).ok()
}

// ── Lecture initiale des vCPUs (premier cycle uniquement) ─────────────────────
pub fn get_current_vcpus(vmid: u32) -> Option<u32> {
    let cfg = get_vm_config(vmid)?;
    if let Some(v) = cfg["vcpus"].as_u64() {
        return Some(v as u32);
    }
    let cores   = cfg["cores"].as_u64().unwrap_or(1) as u32;
    let sockets = cfg["sockets"].as_u64().unwrap_or(1) as u32;
    Some(cores * sockets)
}

// ── Plafond hardware : cores × sockets ───────────────────────────────────────
pub fn get_vm_cores_max(vmid: u32) -> u32 {
    let cfg = match get_vm_config(vmid) { Some(c) => c, None => return 1 };
    let cores   = cfg["cores"].as_u64().unwrap_or(1) as u32;
    let sockets = cfg["sockets"].as_u64().unwrap_or(1) as u32;
    cores * sockets
}

// ── PID du processus QEMU d'une VM ───────────────────────────────────────────
// Nécessaire pour lire /proc/{pid}/task depuis l'hôte.
pub fn get_vm_pid(vmid: u32) -> Option<u32> {
    let out = run_qm(&["status", &vmid.to_string(), "--verbose"])?;
    for line in out.lines() {
        let line = line.trim();
        if line.starts_with("pid:") {
            return line["pid:".len()..].trim().parse::<u32>().ok();
        }
    }
    None
}

// ── Lecture des ticks d'un thread depuis /proc ────────────────────────────────
fn read_thread_ticks(path: &str) -> Option<u64> {
    let stat = std::fs::read_to_string(path).ok()?;
    let f: Vec<&str> = stat.split_whitespace().collect();
    if f.len() < 15 { return None; }
    Some(f[13].parse::<u64>().ok()? + f[14].parse::<u64>().ok()?)
}

// ── Mesure CPU via /proc (méthode hôte, sans agent guest) ────────────────────
// Lit les ticks CPU du processus QEMU directement depuis /proc/{pid}/task.
// Delta sur 400ms → usage instantané, réactif, sans dépendance guest.
// Fonctionne uniquement sur le nœud local (pas cross-node cluster).
// Normalisation : dp/dt * host_cpus / vcpus → ratio [0.0, 1.0] par vCPU.
fn get_vm_cpu_proc(vmid: u32) -> Option<f64> {
    let pid = get_vm_pid(vmid)?;

    let snap = |p: u32| -> Option<(u64, u64)> {
        let mut ticks: u64 = 0;
        if let Ok(entries) = std::fs::read_dir(format!("/proc/{}/task", p)) {
            for e in entries.filter_map(Result::ok) {
                if let Ok(tid) = e.file_name().to_string_lossy().parse::<u32>() {
                    let path = format!("/proc/{}/task/{}/stat", p, tid);
                    if let Some(t) = read_thread_ticks(&path) {
                        ticks += t;
                    }
                }
            }
        }
        let stat = std::fs::read_to_string("/proc/stat").ok()?;
        let total: u64 = stat
            .lines()
            .next()?
            .split_whitespace()
            .skip(1)
            .filter_map(|s| s.parse::<u64>().ok())
            .sum();
        Some((ticks, total))
    };

    let (p1, t1) = snap(pid)?;
    std::thread::sleep(std::time::Duration::from_millis(400));
    let (p2, t2) = snap(pid)?;

    let dp = p2.saturating_sub(p1) as f64;
    let dt = t2.saturating_sub(t1) as f64;
    if dt == 0.0 { return None; }

    let host_cpus = get_host_cpus() as f64;
    let vcpus = get_current_vcpus(vmid).unwrap_or(1) as f64;

    Some((dp / dt * host_cpus / vcpus).clamp(0.0, 1.0))
}

// ── Mesure CPU : cascade /proc → pvesh ───────────────────────────────────────
// Méthode 1 — /proc (hôte local) : instantané, réactif, sans agent guest.
//             Échoue si le PID QEMU est introuvable (VM pausée, etc.).
// Méthode 2 — pvesh status/current : fallback universel.
//             Attention : le champ "cpu" est une moyenne glissante Proxmox
//             sur ~60s — réagit lentement aux pics. Utilisé uniquement si
//             /proc échoue.
// Retourne (usage, timestamp_ms) — le timestamp permet au cycle de rejeter
// les mesures antérieures au début du cycle courant.
pub fn get_vm_cpu_usage(vmid: u32) -> Option<(f64, u64)> {
    let ts_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    // Méthode 1 : /proc — résultat instantané
    if let Some(usage) = get_vm_cpu_proc(vmid) {
        return Some((usage, ts_ms));
    }

    // Méthode 2 : pvesh — fallback (moyenne lente, mais universel)
    let path = format!("/nodes/{}/qemu/{}/status/current", *NODE_NAME, vmid);
    let out  = run_pvesh(&["get", &path, "--output-format=json"])?;
    let val: Value = serde_json::from_str(&out).ok()?;
    if val["status"].as_str().unwrap_or("") != "running" { return None; }
    let cpu = val["cpu"].as_f64().unwrap_or(0.0);
    Some((cpu.clamp(0.0, 1.0), ts_ms))
}

// ── Nombre de CPUs physiques de l'hôte ───────────────────────────────────────
pub fn get_host_cpus() -> u32 {
    Command::new("nproc")
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                String::from_utf8(o.stdout)
                    .ok()
                    .and_then(|s| s.trim().parse().ok())
            } else {
                None
            }
        })
        .unwrap_or(1)
}

// ── Modification des vCPUs (hotplug natif QEMU/KVM) ──────────────────────────
pub fn set_vm_vcpus(vmid: u32, vcpus: u32) -> Option<String> {
    let cores_max = get_vm_cores_max(vmid);
    if vcpus > cores_max {
        crate::logger::log_message(&format!(
            "❌ set_vm_vcpus VM {} : {} > cores_max {} — ignoré",
            vmid, vcpus, cores_max
        ));
        return None;
    }
    if vcpus == 0 {
        crate::logger::log_message(&format!(
            "❌ set_vm_vcpus VM {} : vcpus=0 invalide — ignoré", vmid
        ));
        return None;
    }

    if let Some(cfg) = get_vm_config(vmid) {
        let vcpus_config = cfg["vcpus"].as_u64().unwrap_or(0) as u32;
        if vcpus_config != 0 && vcpus_config != vcpus {
            let path = format!("/nodes/{}/qemu/{}/status/current", *NODE_NAME, vmid);
            if let Some(out) = run_pvesh(&["get", &path, "--output-format=json"]) {
                if let Ok(val) = serde_json::from_str::<Value>(&out) {
                    let vcpus_qemu = val["cpus"].as_u64().unwrap_or(0) as u32;
                    if vcpus_qemu > 0 && vcpus_qemu != vcpus_config {
                        crate::logger::log_message(&format!(
                            "🔧 VM {} désynchronisée (config={} QEMU={}) — resync avant set",
                            vmid, vcpus_config, vcpus_qemu
                        ));
                        let _ = run_qm(&[
                            "set", &vmid.to_string(),
                            "--vcpus", &vcpus_qemu.to_string(),
                        ]);
                    }
                }
            }
        }
    }

    let vmid_s  = vmid.to_string();
    let vcpus_s = vcpus.to_string();
    run_qm(&["set", &vmid_s, "--vcpus", &vcpus_s])
}

// ── Nom de la VM (uniquement pour detect_profile) ─────────────────────────────
pub fn get_vm_name(vmid: u32) -> Option<String> {
    let cfg = get_vm_config(vmid)?;
    cfg["name"].as_str().map(|s| s.to_string())
}

// ── ostype et ISO (pour detect_profile) ──────────────────────────────────────
pub fn get_vm_ostype(vmid: u32) -> Option<String> {
    let cfg = get_vm_config(vmid)?;
    cfg["ostype"].as_str().map(|s| s.to_string())
}

pub fn get_iso_filename(vmid: u32) -> Option<String> {
    let cfg = get_vm_config(vmid)?;
    if let Some(obj) = cfg.as_object() {
        for (_key, value) in obj {
            if let Some(s) = value.as_str() {
                if let Some(pos) = s.find("iso/") {
                    let rest = &s[pos + 4..];
                    return Some(rest.split(',').next().unwrap_or(rest).to_string());
                }
            }
        }
    }
    None
}

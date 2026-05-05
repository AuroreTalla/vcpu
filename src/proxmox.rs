use serde_json::Value;
use std::process::Command;

pub fn run_command(cmd: &str) -> Option<String> {
    let output = Command::new("sh").arg("-c").arg(cmd).output().ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

pub fn get_all_vms() -> Vec<u32> {
    let out = match run_command(r#"pvesh get /nodes/localhost/qemu --output-format=json"#) {
        Some(o) => o,
        None    => return vec![],
    };
    let vms: Vec<Value> = match serde_json::from_str(&out) {
        Ok(v)  => v,
        Err(_) => return vec![],
    };
    vms.iter()
        .filter_map(|v| v["vmid"].as_u64().map(|id| id as u32))
        .collect()
}

pub fn get_vm_config(vmid: u32) -> Option<Value> {
    let cmd = format!("pvesh get /nodes/localhost/qemu/{}/config --output-format=json", vmid);
    let out = run_command(&cmd)?;
    serde_json::from_str(&out).ok()
}

pub fn get_current_vcpus(vmid: u32) -> Option<u32> {
    let cfg = get_vm_config(vmid)?;
    cfg["vcpus"].as_u64().map(|v| v as u32)
}

pub fn get_vm_name(vmid: u32) -> Option<String> {
    let cfg = get_vm_config(vmid)?;
    cfg["name"].as_str().map(|s| s.to_string())
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

pub fn get_vm_ostype(vmid: u32) -> Option<String> {
    let cfg = get_vm_config(vmid)?;
    cfg["ostype"].as_str().map(|s| s.to_string())
}

pub fn get_vm_pid(vmid: u32) -> Option<u32> {
    let out = run_command("qm list")?;
    for line in out.lines().skip(1) {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 6 {
            if let Ok(id) = parts[0].parse::<u32>() {
                if id == vmid {
                    return parts[5].parse::<u32>().ok();
                }
            }
        }
    }
    None
}

// ── Tag opt-in ────────────────────────────────────────────────────────────────
// Sans aucun tag sur aucune VM → toutes incluses (comportement par défaut)
// Dès qu'une VM a le tag "vcpu-agent" → seules les VMs taguées participent
pub fn vm_has_agent_tag(vmid: u32) -> bool {
    let cfg = match get_vm_config(vmid) {
        Some(c) => c,
        None    => return false,
    };
    match cfg["tags"].as_str() {
        None       => true,
        Some("")   => true,
        Some(tags) => tags.split(';').any(|t| t.trim() == "vcpu-agent"),
    }
}

// ── Mesure CPU — 3 niveaux de priorité ───────────────────────────────────────
pub fn get_vm_cpu_usage(vmid: u32) -> f64 {
    // PRIORITÉ 1 : QEMU Guest Agent — précis, fonctionne en nested ET production
    if let Some(usage) = get_vm_cpu_via_agent(vmid) {
        return usage;
    }

    // PRIORITÉ 2 : pvesh — production bare-metal sans agent installé
    let cmd = format!(
        "pvesh get /nodes/localhost/qemu/{}/status/current --output-format=json",
        vmid
    );
    if let Some(out) = run_command(&cmd) {
        if let Ok(val) = serde_json::from_str::<Value>(&out) {
            let cpu = val["cpu"].as_f64().unwrap_or(0.0);
            if cpu > 0.005 {
                return cpu.clamp(0.0, 1.0);
            }
        }
    }

    // PRIORITÉ 3 : /proc via l'hôte — nested virt sans agent
    get_vm_cpu_proc(vmid)
}

// Lecture CPU via QEMU Guest Agent
// Lit /proc/stat depuis l'intérieur de la VM → mesure exacte indépendante de l'hôte
fn get_vm_cpu_via_agent(vmid: u32) -> Option<f64> {
    let snap1 = get_agent_cpu_snapshot(vmid)?;
    std::thread::sleep(std::time::Duration::from_millis(300));
    let snap2 = get_agent_cpu_snapshot(vmid)?;

    let delta_used  = snap2.0.saturating_sub(snap1.0) as f64;
    let delta_total = snap2.1.saturating_sub(snap1.1) as f64;

    if delta_total == 0.0 { return None; }

    // delta_used/delta_total = fraction du temps CPU utilisée dans la VM
    // déjà normalisé par rapport aux vCPUs de la VM, pas besoin de l'hôte
    Some((delta_used / delta_total).clamp(0.0, 1.0))
}

// Snapshot (used_ticks, total_ticks) via qm guest exec → /proc/stat de la VM
fn get_agent_cpu_snapshot(vmid: u32) -> Option<(u64, u64)> {
    let cmd = format!(
        "qm guest exec {} -- sh -c 'cat /proc/stat' 2>/dev/null",
        vmid
    );
    let out = run_command(&cmd)?;

    // qm guest exec retourne du JSON :
    // {"exitcode":0,"out-data":"cpu  1234 0 567 8910 ...\n","err-data":""}
    // OU si async : {"pid":123} → nécessite un poll
    let val: Value = serde_json::from_str(&out).ok()?;

    let line = if let Some(data) = val["out-data"].as_str() {
        // Réponse synchrone directe
        data.lines().next()?.to_string()
    } else if let Some(pid) = val["pid"].as_u64() {
        // Réponse asynchrone → poll du résultat
        std::thread::sleep(std::time::Duration::from_millis(100));
        let poll = format!("qm guest exec-status {} {} 2>/dev/null", vmid, pid);
        let poll_out = run_command(&poll)?;
        let poll_val: Value = serde_json::from_str(&poll_out).ok()?;
        poll_val["out-data"].as_str()?.lines().next()?.to_string()
    } else {
        return None;
    };

    // Parser "cpu  user nice system idle iowait irq softirq steal guest guest_nice"
    let fields: Vec<u64> = line
        .split_whitespace()
        .skip(1) // sauter le mot "cpu"
        .filter_map(|s| s.parse().ok())
        .collect();

    if fields.len() < 4 { return None; }

    let idle   = fields[3];
    let iowait = fields.get(4).copied().unwrap_or(0);
    let total: u64 = fields.iter().sum();
    let used   = total.saturating_sub(idle).saturating_sub(iowait);

    Some((used, total))
}

// Fallback /proc via l'hôte — utilisé uniquement si agent indisponible
fn get_vm_cpu_proc(vmid: u32) -> f64 {
    let pid = match get_vm_pid(vmid) {
        Some(p) => p,
        None    => return 0.0,
    };

    let snapshot = |p: u32| -> Option<(u64, u64)> {
        let mut proc_ticks: u64 = 0;
        let task_dir = format!("/proc/{}/task", p);
        if let Ok(entries) = std::fs::read_dir(&task_dir) {
            for entry in entries.filter_map(Result::ok) {
                if let Ok(tid) = entry.file_name().to_string_lossy().parse::<u32>() {
                    let path = format!("/proc/{}/task/{}/stat", p, tid);
                    if let Some(t) = read_single_thread_ticks(&path) {
                        proc_ticks += t;
                    }
                }
            }
        }
        let stat  = std::fs::read_to_string("/proc/stat").ok()?;
        let total: u64 = stat.lines().next()?
            .split_whitespace().skip(1)
            .filter_map(|s| s.parse::<u64>().ok()).sum();
        Some((proc_ticks, total))
    };

    let (p1, t1) = match snapshot(pid) { Some(v) => v, None => return 0.0 };
    std::thread::sleep(std::time::Duration::from_millis(300));
    let (p2, t2) = match snapshot(pid) { Some(v) => v, None => return 0.0 };

    let dp = p2.saturating_sub(p1) as f64;
    let dt = t2.saturating_sub(t1) as f64;
    if dt == 0.0 { return 0.0; }

    let host_cpus = get_host_cpus() as f64;
    let vcpus     = get_current_vcpus(vmid).unwrap_or(1) as f64;

    (dp / dt * host_cpus / vcpus).clamp(0.0, 2.0)
}

fn read_single_thread_ticks(stat_path: &str) -> Option<u64> {
    let stat   = std::fs::read_to_string(stat_path).ok()?;
    let fields: Vec<&str> = stat.split_whitespace().collect();
    if fields.len() < 15 { return None; }
    let utime: u64 = fields[13].parse().ok()?;
    let stime: u64 = fields[14].parse().ok()?;
    Some(utime + stime)
}

pub fn get_host_cpus() -> u32 {
    run_command("nproc")
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(1)
}

pub fn set_vm_vcpus(vmid: u32, vcpus: u32) -> Option<String> {
    let cmd = format!("qm set {} --vcpus {}", vmid, vcpus);
    run_command(&cmd)
}

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
    serde_json::from_str(&run_command(&cmd)?).ok()
}

pub fn get_current_vcpus(vmid: u32) -> Option<u32> {
    let cfg = get_vm_config(vmid)?;
    cfg["vcpus"].as_u64().map(|v| v as u32)
}

// Plafond hardware : cores × sockets
// set --vcpus ne peut pas dépasser cette valeur
pub fn get_vm_cores_max(vmid: u32) -> u32 {
    let cfg = match get_vm_config(vmid) { Some(c) => c, None => return 1 };
    let cores   = cfg["cores"].as_u64().unwrap_or(1) as u32;
    let sockets = cfg["sockets"].as_u64().unwrap_or(1) as u32;
    cores * sockets
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
                if id == vmid { return parts[5].parse::<u32>().ok(); }
            }
        }
    }
    None
}

// ── Tag opt-in ────────────────────────────────────────────────────────────────
// Sans tag → toutes les VMs incluses
// Avec tag "vcpu-agent" → seulement les VMs taguées
pub fn vm_has_agent_tag(vmid: u32) -> bool {
    let cfg = match get_vm_config(vmid) { Some(c) => c, None => return false };
    match cfg["tags"].as_str() {
        None       => true,
        Some("")   => true,
        Some(tags) => tags.split(';').any(|t| t.trim() == "vcpu-agent"),
    }
}

// ── Mesure CPU : 3 méthodes en cascade ───────────────────────────────────────
pub fn get_vm_cpu_usage(vmid: u32) -> f64 {
    // 1. Agent QEMU — mesure depuis l'intérieur, précis, nested + production
    if let Some(u) = get_vm_cpu_via_agent(vmid) { return u; }
    // 2. pvesh — production bare-metal sans agent
    let cmd = format!(
        "pvesh get /nodes/localhost/qemu/{}/status/current --output-format=json", vmid
    );
    if let Some(out) = run_command(&cmd) {
        if let Ok(val) = serde_json::from_str::<Value>(&out) {
            let cpu = val["cpu"].as_f64().unwrap_or(0.0);
            if cpu > 0.005 { return cpu.clamp(0.0, 1.0); }
        }
    }
    // 3. /proc via hôte — fallback nested sans agent
    get_vm_cpu_proc(vmid)
}

// Script shell embarqué : fait le delta /proc/stat en 400ms dans la VM
// → 1 seul appel réseau au lieu de 2, résultat direct
fn get_vm_cpu_via_agent(vmid: u32) -> Option<f64> {
    let script = "s1=$(awk 'NR==1{u=$2+$4;t=$2+$3+$4+$5+$6+$7+$8;print u\" \"t}' /proc/stat);sleep 0.4;s2=$(awk 'NR==1{u=$2+$4;t=$2+$3+$4+$5+$6+$7+$8;print u\" \"t}' /proc/stat);u1=${s1%% *};t1=${s1##* };u2=${s2%% *};t2=${s2##* };dt=$((t2-t1));du=$((u2-u1));[ $dt -gt 0 ]&&echo \"$du $dt\"||echo \"0 1\"";

    let cmd = format!("qm guest exec {} -- sh -c '{}' 2>/dev/null", vmid, script);
    let out = run_command(&cmd)?;
    let val: Value = serde_json::from_str(&out).ok()?;

    let data = if let Some(d) = val["out-data"].as_str() {
        d.trim().to_string()
    } else if let Some(pid) = val["pid"].as_u64() {
        // Réponse asynchrone → attendre et poller
        std::thread::sleep(std::time::Duration::from_millis(500));
        let poll_out = run_command(&format!("qm guest exec-status {} {} 2>/dev/null", vmid, pid))?;
        let pv: Value = serde_json::from_str(&poll_out).ok()?;
        pv["out-data"].as_str()?.trim().to_string()
    } else {
        return None;
    };

    let parts: Vec<u64> = data.split_whitespace()
        .filter_map(|s| s.parse().ok()).collect();
    if parts.len() < 2 || parts[1] == 0 { return None; }
    Some((parts[0] as f64 / parts[1] as f64).clamp(0.0, 1.0))
}

fn get_vm_cpu_proc(vmid: u32) -> f64 {
    let pid = match get_vm_pid(vmid) { Some(p) => p, None => return 0.0 };

    let snap = |p: u32| -> Option<(u64, u64)> {
        let mut ticks: u64 = 0;
        if let Ok(entries) = std::fs::read_dir(format!("/proc/{}/task", p)) {
            for e in entries.filter_map(Result::ok) {
                if let Ok(tid) = e.file_name().to_string_lossy().parse::<u32>() {
                    if let Some(t) = read_thread_ticks(&format!("/proc/{}/task/{}/stat", p, tid)) {
                        ticks += t;
                    }
                }
            }
        }
        let stat  = std::fs::read_to_string("/proc/stat").ok()?;
        let total: u64 = stat.lines().next()?
            .split_whitespace().skip(1)
            .filter_map(|s| s.parse::<u64>().ok()).sum();
        Some((ticks, total))
    };

    let (p1, t1) = match snap(pid) { Some(v) => v, None => return 0.0 };
    std::thread::sleep(std::time::Duration::from_millis(400));
    let (p2, t2) = match snap(pid) { Some(v) => v, None => return 0.0 };

    let dp = p2.saturating_sub(p1) as f64;
    let dt = t2.saturating_sub(t1) as f64;
    if dt == 0.0 { return 0.0; }

    let host_cpus = get_host_cpus() as f64;
    let vcpus     = get_current_vcpus(vmid).unwrap_or(1) as f64;
    (dp / dt * host_cpus / vcpus).clamp(0.0, 2.0)
}

fn read_thread_ticks(path: &str) -> Option<u64> {
    let stat = std::fs::read_to_string(path).ok()?;
    let f: Vec<&str> = stat.split_whitespace().collect();
    if f.len() < 15 { return None; }
    Some(f[13].parse::<u64>().ok()? + f[14].parse::<u64>().ok()?)
}

pub fn get_host_cpus() -> u32 {
    run_command("nproc").and_then(|s| s.trim().parse().ok()).unwrap_or(1)
}

// Synchrone — retourne Option<String> pour pouvoir vérifier le succès
pub fn set_vm_vcpus(vmid: u32, vcpus: u32) -> Option<String> {
    run_command(&format!("qm set {} --vcpus {}", vmid, vcpus))
}

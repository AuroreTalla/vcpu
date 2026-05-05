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

// Nombre max de vCPUs utilisables = cores × sockets
// C'est le plafond hardware — on ne peut pas dépasser ça avec hotplug
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
                if id == vmid {
                    return parts[5].parse::<u32>().ok();
                }
            }
        }
    }
    None
}

// ── Tag opt-in ────────────────────────────────────────────────────────────────
pub fn vm_has_agent_tag(vmid: u32) -> bool {
    let cfg = match get_vm_config(vmid) { Some(c) => c, None => return false };
    match cfg["tags"].as_str() {
        None       => true,
        Some("")   => true,
        Some(tags) => tags.split(';').any(|t| t.trim() == "vcpu-agent"),
    }
}

// ── Mesure CPU ────────────────────────────────────────────────────────────────
// Priorité 1 : agent QEMU (mesure depuis l'intérieur de la VM)
// Priorité 2 : pvesh (production bare-metal sans agent)
// Priorité 3 : /proc via l'hôte (nested virt sans agent)
pub fn get_vm_cpu_usage(vmid: u32) -> f64 {
    if let Some(u) = get_vm_cpu_via_agent(vmid) { return u; }

    let cmd = format!(
        "pvesh get /nodes/localhost/qemu/{}/status/current --output-format=json", vmid
    );
    if let Some(out) = run_command(&cmd) {
        if let Ok(val) = serde_json::from_str::<Value>(&out) {
            let cpu = val["cpu"].as_f64().unwrap_or(0.0);
            if cpu > 0.005 { return cpu.clamp(0.0, 1.0); }
        }
    }

    get_vm_cpu_proc(vmid)
}

// Mesure via agent QEMU — UN SEUL appel shell qui fait le delta en interne
// Le script shell lit /proc/stat deux fois avec 400ms d'écart et calcule l'usage
// → 1 appel réseau au lieu de 2, résultat direct en pourcentage
fn get_vm_cpu_via_agent(vmid: u32) -> Option<f64> {
    // Script shell embarqué qui calcule le delta CPU directement dans la VM
    let script = r#"
s1=$(cat /proc/stat | head -1 | awk '{u=$2+$4; t=$2+$3+$4+$5+$6+$7+$8; print u " " t}');
sleep 0.4;
s2=$(cat /proc/stat | head -1 | awk '{u=$2+$4; t=$2+$3+$4+$5+$6+$7+$8; print u " " t}');
u1=$(echo $s1 | cut -d' ' -f1); t1=$(echo $s1 | cut -d' ' -f2);
u2=$(echo $s2 | cut -d' ' -f1); t2=$(echo $s2 | cut -d' ' -f2);
dt=$((t2-t1)); du=$((u2-u1));
if [ $dt -gt 0 ]; then echo "$du $dt"; else echo "0 1"; fi
"#;

    let cmd = format!(
        "qm guest exec {} -- sh -c '{}' 2>/dev/null",
        vmid,
        script.replace('\n', " ").trim()
    );

    let out = run_command(&cmd)?;
    let val: Value = serde_json::from_str(&out).ok()?;

    // Gérer réponse synchrone et asynchrone
    let data = if let Some(d) = val["out-data"].as_str() {
        d.trim().to_string()
    } else if let Some(pid) = val["pid"].as_u64() {
        std::thread::sleep(std::time::Duration::from_millis(600));
        let poll = format!("qm guest exec-status {} {} 2>/dev/null", vmid, pid);
        let poll_out = run_command(&poll)?;
        let poll_val: Value = serde_json::from_str(&poll_out).ok()?;
        poll_val["out-data"].as_str()?.trim().to_string()
    } else {
        return None;
    };

    // Résultat attendu : "du dt" (delta_used delta_total)
    let parts: Vec<u64> = data.split_whitespace()
        .filter_map(|s| s.parse().ok())
        .collect();

    if parts.len() < 2 || parts[1] == 0 { return None; }

    let usage = (parts[0] as f64 / parts[1] as f64).clamp(0.0, 1.0);
    Some(usage)
}

// Fallback /proc via l'hôte — nested virt sans agent
fn get_vm_cpu_proc(vmid: u32) -> f64 {
    let pid = match get_vm_pid(vmid) { Some(p) => p, None => return 0.0 };

    let snapshot = |p: u32| -> Option<(u64, u64)> {
        let mut ticks: u64 = 0;
        if let Ok(entries) = std::fs::read_dir(format!("/proc/{}/task", p)) {
            for e in entries.filter_map(Result::ok) {
                if let Ok(tid) = e.file_name().to_string_lossy().parse::<u32>() {
                    let path = format!("/proc/{}/task/{}/stat", p, tid);
                    if let Some(t) = read_thread_ticks(&path) { ticks += t; }
                }
            }
        }
        let stat  = std::fs::read_to_string("/proc/stat").ok()?;
        let total: u64 = stat.lines().next()?
            .split_whitespace().skip(1)
            .filter_map(|s| s.parse::<u64>().ok()).sum();
        Some((ticks, total))
    };

    let (p1, t1) = match snapshot(pid) { Some(v) => v, None => return 0.0 };
    std::thread::sleep(std::time::Duration::from_millis(400));
    let (p2, t2) = match snapshot(pid) { Some(v) => v, None => return 0.0 };

    let dp = p2.saturating_sub(p1) as f64;
    let dt = t2.saturating_sub(t1) as f64;
    if dt == 0.0 { return 0.0; }

    let host_cpus = get_host_cpus() as f64;
    let vcpus     = get_current_vcpus(vmid).unwrap_or(1) as f64;
    (dp / dt * host_cpus / vcpus).clamp(0.0, 2.0)
}

fn read_thread_ticks(path: &str) -> Option<u64> {
    let stat   = std::fs::read_to_string(path).ok()?;
    let fields: Vec<&str> = stat.split_whitespace().collect();
    if fields.len() < 15 { return None; }
    let u: u64 = fields[13].parse().ok()?;
    let s: u64 = fields[14].parse().ok()?;
    Some(u + s)
}

pub fn get_host_cpus() -> u32 {
    run_command("nproc").and_then(|s| s.trim().parse().ok()).unwrap_or(1)
}

pub fn set_vm_vcpus(vmid: u32, vcpus: u32) -> Option<String> {
    run_command(&format!("qm set {} --vcpus {}", vmid, vcpus))
}

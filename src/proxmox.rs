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
        None => return vec![],
    };
    let vms: Vec<Value> = match serde_json::from_str(&out) {
        Ok(v) => v,
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

// ====================== DÉTECTION CPU (nested + production) ======================
pub fn get_vm_cpu_usage(vmid: u32) -> f64 {
    // 1. Priorité : pvesh (fonctionne bien en production bare-metal)
    let cmd = format!(
        "pvesh get /nodes/localhost/qemu/{}/status/current --output-format=json",
        vmid
    );
    if let Some(out) = run_command(&cmd) {
        if let Ok(val) = serde_json::from_str::<Value>(&out) {
            let cpu_raw = val["cpu"].as_f64().unwrap_or(0.0);
            let maxcpu = val["cpus"].as_f64().unwrap_or(1.0);

            if cpu_raw > 0.005 {
                let usage = cpu_raw / maxcpu;   // valeur correcte (peut dépasser 1.0)
                
                return usage;
            }
        }
    }

    // 2. Fallback pour virtualisation imbriquée (nested)
    get_vm_cpu_proc(vmid)
}

fn get_vm_cpu_proc(vmid: u32) -> f64 {
    let pid = match get_vm_pid(vmid) {
        Some(p) => p,
        None => return 0.0,
    };

    let read_snapshot = || -> Option<(u64, u64)> {
        let mut proc_ticks: u64 = 0;
        let task_dir = format!("/proc/{}/task", pid);

        if let Ok(entries) = std::fs::read_dir(&task_dir) {
            for entry in entries.filter_map(Result::ok) {
                if let Ok(tid) = entry.file_name().to_string_lossy().parse::<u32>() {
                    let stat_path = format!("/proc/{}/task/{}/stat", pid, tid);
                    if let Some(ticks) = read_single_thread_ticks(&stat_path) {
                        proc_ticks += ticks;
                    }
                }
            }
        }

        let cpu_stat = std::fs::read_to_string("/proc/stat").ok()?;
        let first_line = cpu_stat.lines().next()?;
        let total_ticks: u64 = first_line
            .split_whitespace()
            .skip(1)
            .filter_map(|s| s.parse::<u64>().ok())
            .sum();

        Some((proc_ticks, total_ticks))
    };

    let (p1, t1) = match read_snapshot() { Some(v) => v, None => return 0.0 };
    std::thread::sleep(std::time::Duration::from_millis(500));
    let (p2, t2) = match read_snapshot() { Some(v) => v, None => return 0.0 };

    let delta_proc = p2.saturating_sub(p1) as f64;
    let delta_total = t2.saturating_sub(t1) as f64;
    if delta_total == 0.0 {
        return 0.0;
    }

    let host_cpus = get_host_cpus() as f64;
    let vcpus = get_current_vcpus(vmid).unwrap_or(1) as f64;

    // Calcul précis sans limiter à 100%
    let usage = delta_proc / delta_total * host_cpus / vcpus;

    usage
}

fn read_single_thread_ticks(stat_path: &str) -> Option<u64> {
    let stat = std::fs::read_to_string(stat_path).ok()?;
    let fields: Vec<&str> = stat.split_whitespace().collect();
    if fields.len() < 15 {
        return None;
    }
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

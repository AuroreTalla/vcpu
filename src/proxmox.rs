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

pub fn get_vm_status(vmid: u32) -> Option<Value> {
    let cmd = format!("pvesh get /nodes/localhost/qemu/{}/status/current --output-format=json", vmid);
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
                    if let Some(comma_pos) = rest.find(',') {
                        return Some(rest[..comma_pos].to_string());
                    } else {
                        return Some(rest.to_string());
                    }
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

use std::fs::{self, File};
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

const SIGNALS_DIR: &str = "/var/lib/live-migrator/signals";

#[derive(Debug, Clone, Copy)]
pub enum Urgency {
    High,
    Medium,
    Low,
    Critical,
}

impl Urgency {
    pub fn as_str(&self) -> &'static str {
        match self {
            Urgency::Low => "low",
            Urgency::Medium => "medium",
            Urgency::High => "high",
            Urgency::Critical => "critical",
        }
    }
}

pub fn send_migrate_vm(vmid: u32, reason: String, urgency: Urgency) -> bool {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let filename = format!("signal_{}_migrate_vm.sig", ts);
    let tmp_path = format!("{}/{}.tmp", SIGNALS_DIR, filename);
    let final_path = format!("{}/{}", SIGNALS_DIR, filename);

    let content = format!(
        "type=MIGRATE_VM\n\
         vmid={}\n\
         source_agent=vcpu-agent\n\
         reason={}\n\
         urgency={}\n\
         timestamp={}\n",
        vmid,
        reason.replace('\n', " ").replace('=', "-"),
        urgency.as_str(),
        chrono::Local::now().to_rfc3339()
    );

    if let Err(e) = fs::create_dir_all(SIGNALS_DIR) {
        crate::logger::log_message(&format!("❌ [signals] Création dossier impossible: {}", e));
        return false;
    }

    match File::create(&tmp_path).and_then(|mut f| f.write_all(content.as_bytes())) {
        Ok(_) => {
            if fs::rename(&tmp_path, &final_path).is_ok() {
                crate::logger::log_message(&format!(
                    "📤 SIGNAL → MIGRATE_VM VM{} | {} | urgency={}", 
                    vmid, reason, urgency.as_str()
                ));
                return true;
            }
        }
        Err(e) => {
            let _ = fs::remove_file(&tmp_path);
            crate::logger::log_message(&format!("❌ [signals] Échec écriture: {}", e));
        }
    }
    false
}

pub fn send_lighten_node(reason: String, urgency: Urgency) -> bool {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let filename  = format!("signal_{}_lighten_node.sig", ts);
    let tmp_path  = format!("{}/{}.tmp", SIGNALS_DIR, filename);
    let final_path = format!("{}/{}", SIGNALS_DIR, filename);
    let content = format!(
        "type=LIGHTEN_NODE\n\
         source_agent=vcpu-agent\n\
         reason={}\n\
         urgency={}\n\
         resource=cpu\n\
         timestamp={}\n",
        reason.replace('\n', " ").replace('=', "-"),
        urgency.as_str(),
        chrono::Local::now().to_rfc3339()
    );
    if let Err(e) = fs::create_dir_all(SIGNALS_DIR) {
        crate::logger::log_message(&format!("❌ [signals] Dossier impossible: {}", e));
        return false;
    }
    match File::create(&tmp_path).and_then(|mut f| f.write_all(content.as_bytes())) {
        Ok(_) => {
            if fs::rename(&tmp_path, &final_path).is_ok() {
                crate::logger::log_message(&format!(
                    "📤 SIGNAL → LIGHTEN_NODE | {} | urgency={}", reason, urgency.as_str()
                ));
                return true;
            }
        }
        Err(e) => {
            let _ = fs::remove_file(&tmp_path);
            crate::logger::log_message(&format!("❌ [signals] Échec: {}", e));
        }
    }
    false
}

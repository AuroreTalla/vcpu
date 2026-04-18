use chrono::Local;
use std::fs::OpenOptions;
use std::io::Write;

pub fn log_message(msg: &str) {
    let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S");
    let line = format!("{} - {}\n", timestamp, msg);
    println!("{}", line.trim());
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open("/var/log/vcpu-balancer.log") {
        let _ = f.write_all(line.as_bytes());
    }
}

pub fn log_debug(msg: &str) {
    log_message(&format!("[DEBUG] {}", msg));
}

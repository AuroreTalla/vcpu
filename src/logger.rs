use chrono::Local;
use std::fs::OpenOptions;
use std::io::Write;

pub fn log_message(msg: &str) {
    let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let line = format!("{} - {}\n", timestamp, msg);
    println!("{}", line.trim());

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open("/var/log/vcpu-balancer.log")
        .unwrap();
    file.write_all(line.as_bytes()).unwrap();
}

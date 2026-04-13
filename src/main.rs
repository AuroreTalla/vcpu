mod config;
mod logger;
mod proxmox;
mod vm_recognizer;
mod cpu_balancer;

fn main() {
    let config = config::load();
    cpu_balancer::run(config);
}

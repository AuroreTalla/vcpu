# Agent vCPU

**Agent Rust intelligent pour l'équilibrage dynamique des vCPU dans un cluster Proxmox.**

Cet agent surveille en temps réel l'utilisation des vCPU des machines virtuelles et permet un **prêt/ré-emprunt automatique** de vcpu entre VMs sur un même nœud et déclenche une **live migration**  afin de réduire les situations de surcharge.

---

## Fonctionnalités

- **Surveillance précise** du taux d'utilisation vCPU
- **Prêt dynamique de vCPU** : une VM en détresse emprunte des cœurs à une VM peu chargée
- **Déclenchement de live migration** 
- **Remboursement automatique** une fois la charge redescendue ou la VM migrée
- **Nettoyage des prêts** après migration (les ressources restent sur le nœud)
- **Configuration par profil** (selon OS / ISO)
- **Opt-in / Opt-out** par VM via tag `vcpu-agent`
- **Respect des limites** min/max par VM

---

## Prérequis

- Proxmox VE (testé sur 8.x)
- Rust 1.70+ et Cargo
- QEMU Guest Agent recommandé dans les VMs (pour une mesure plus précise)
- Au moins 2 VMs avec le tag `vcpu-agent`

---

## Installation

### 1. Compilation

```
git clone <lien>
cd agent-vcpu
cargo build --release
```

### 2. Installation du binaire
```
sudo cp target/release/agent-vcpu /usr/local/bin/agent-vcpu
tail -f /var/log/vcpu-balancer.log (consultation des logs détaillés)
```

### 3. Création du fichier de configuration
```
check_interval = 2
window_seconds = 3
duree_avant_action = 3
cpu_overcommit_ratio = 3.0

seuil_detresse = 0.78
seuil_donneuse = 0.52

migration_cooldown_seconds = 300
distress_before_migration = 8

# ====================== Profils ======================
[profiles.web]
iso_pattern = "2026.03.24"
min = 2
max = 8

[profiles.desktop]
iso_pattern = "desktop"
min = 1
max = 4

[profiles.linux]
iso_pattern = ""
min = 2
max = 6
```

## 3. Architecture
- main.rs : Point d'entrée du programme
- cpu_balancer.rs : Logique principale de balancing, prêts et remboursements
- signals.rs : Envoi des signaux vers le daemon live-migrator
- proxmox.rs : Interaction avec Proxmox (qm, pvesh, Guest Agent, etc.)
- vm_recognizer.rs : Détection automatique du profil 
- logger.rs : Logging dans fichier + console

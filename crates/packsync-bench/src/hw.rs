//! What the numbers were measured on.

use std::path::Path;
use std::process::Command;

use serde::Serialize;
use sysinfo::System;

#[derive(Debug, Clone, Serialize, Default)]
pub struct Hardware {
    pub cpu: String,
    pub logical_cores: usize,
    pub ram_gb: f64,
    pub os: String,
    pub kernel: String,
    pub filesystem: String,
    pub device: String,
    pub rotational: Option<bool>,
    pub work_dir: String,
    pub rustc: String,
    pub profile: String,
}

fn run(cmd: &str, args: &[&str]) -> String {
    Command::new(cmd)
        .args(args)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// The mount that contains `dir` (longest matching mount point), Linux only.
fn mount_of(dir: &Path) -> (String, String) {
    let Ok(mounts) = std::fs::read_to_string("/proc/mounts") else { return (String::new(), String::new()) };
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let mut best: Option<(usize, String, String)> = None;
    for l in mounts.lines() {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() < 3 {
            continue;
        }
        let mp = f[1].replace("\\040", " ");
        if dir.starts_with(&mp) && best.as_ref().map(|b| mp.len() > b.0).unwrap_or(true) {
            best = Some((mp.len(), f[0].to_string(), f[2].to_string()));
        }
    }
    best.map(|(_, dev, fs)| (dev, fs)).unwrap_or_default()
}

fn rotational(device: &str) -> Option<bool> {
    let name = device.strip_prefix("/dev/")?;
    // nvme0n1p2 -> nvme0n1, sda1 -> sda
    let base = if name.starts_with("nvme") || name.starts_with("mmcblk") {
        name.rsplit_once('p').map(|(b, _)| b).unwrap_or(name)
    } else {
        name.trim_end_matches(|c: char| c.is_ascii_digit())
    };
    let v = std::fs::read_to_string(format!("/sys/block/{base}/queue/rotational")).ok()?;
    Some(v.trim() == "1")
}

pub fn detect(work_dir: &Path) -> Hardware {
    let mut sys = System::new();
    sys.refresh_memory();
    sys.refresh_cpu_all();
    let cpu = sys.cpus().first().map(|c| c.brand().to_string()).unwrap_or_default();
    let (device, filesystem) = mount_of(work_dir);
    Hardware {
        cpu,
        logical_cores: sys.cpus().len(),
        ram_gb: (sys.total_memory() as f64 / 1e9 * 10.0).round() / 10.0,
        os: format!("{} {}", System::name().unwrap_or_default(), System::os_version().unwrap_or_default()),
        kernel: System::kernel_version().unwrap_or_else(|| run("uname", &["-r"])),
        rotational: rotational(&device),
        filesystem,
        device,
        work_dir: work_dir.display().to_string(),
        rustc: run("rustc", &["--version"]),
        profile: if cfg!(debug_assertions) {
            "debug (numbers are not representative: use --release)".into()
        } else {
            "release".into()
        },
    }
}

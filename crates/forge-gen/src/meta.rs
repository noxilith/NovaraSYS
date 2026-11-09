use serde::{Deserialize, Serialize};

use crate::GenError;

/// What a generation (or the live entry) boots: a kernel and initramfs that
/// live on the boot partition, plus the kernel command line.
///
/// `cmdline` is the *base* command line: no `rootflags=` and no `ro`/`rw`,
/// because those are added per entry (each generation boots its own subvolume).
/// Other `rootflags` options are dropped; mount options come from fstab.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootInfo {
    pub kernel: String,
    pub initramfs: String,
    pub cmdline: String,
}

impl BootInfo {
    pub fn validate(&self) -> Result<(), GenError> {
        let file_ok = |s: &str| {
            !s.is_empty()
                && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'))
        };
        if !file_ok(&self.kernel) {
            return Err(GenError::InvalidBoot(format!("kernel file name `{}`", self.kernel)));
        }
        // An empty name means "no initramfs": the kernel mounts its root itself (the Novara
        // kernel has the root drivers built in), which needs `root=PARTUUID=` rather than UUID.
        if !self.initramfs.is_empty() && !file_ok(&self.initramfs) {
            return Err(GenError::InvalidBoot(format!("initramfs file name `{}`", self.initramfs)));
        }
        if self.cmdline.contains(['\n', '\r']) {
            return Err(GenError::InvalidBoot("command line contains a line break".into()));
        }
        Ok(())
    }
}

/// `<snapshots>/gen-N.toml`, beside the read-only snapshot `gen-N`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    pub id: u32,
    pub description: String,
    /// Seconds since the Unix epoch.
    pub created: u64,
    pub kernel: String,
    pub initramfs: String,
    pub cmdline: String,
    /// Store paths this generation needs, so `forge gc` keeps them (not filled in yet).
    #[serde(default)]
    pub store_paths: Vec<String>,
}

impl Meta {
    pub fn boot(&self) -> BootInfo {
        BootInfo {
            kernel: self.kernel.clone(),
            initramfs: self.initramfs.clone(),
            cmdline: self.cmdline.clone(),
        }
    }
}

/// Strip `rootflags=` and `ro`/`rw` from a raw kernel command line.
pub fn base_cmdline(raw: &str) -> String {
    raw.split_whitespace()
        .filter(|t| *t != "ro" && *t != "rw" && !t.starts_with("rootflags="))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The btrfs subvolume the system booted from, per `rootflags=subvol=...`.
pub fn booted_subvol(raw: &str) -> Option<String> {
    raw.split_whitespace()
        .filter_map(|t| t.strip_prefix("rootflags="))
        .flat_map(|v| v.split(','))
        .find_map(|f| f.strip_prefix("subvol="))
        .map(|s| s.trim_start_matches('/').to_string())
}

/// Base command line plus the root subvolume and mount mode for one entry.
pub fn with_root_subvol(base: &str, subvol: &str, readonly: bool) -> String {
    let mode = if readonly { "ro" } else { "rw" };
    [base, &format!("rootflags=subvol={subvol}"), mode]
        .iter()
        .filter(|s| !s.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_root_flags_and_mode() {
        let raw = "root=UUID=abc rootflags=subvol=@,compress=zstd rw quiet init=/sbin/openrc-init";
        assert_eq!(base_cmdline(raw), "root=UUID=abc quiet init=/sbin/openrc-init");
    }

    #[test]
    fn finds_booted_subvol() {
        assert_eq!(booted_subvol("root=x rootflags=subvol=@ rw").as_deref(), Some("@"));
        assert_eq!(
            booted_subvol("rootflags=compress=zstd,subvol=/@snapshots/gen-3 ro").as_deref(),
            Some("@snapshots/gen-3")
        );
        assert_eq!(booted_subvol("root=x rw"), None);
    }

    #[test]
    fn builds_entry_cmdline() {
        assert_eq!(with_root_subvol("root=x", "@", false), "root=x rootflags=subvol=@ rw");
        assert_eq!(
            with_root_subvol("root=x", "@snapshots/gen-2", true),
            "root=x rootflags=subvol=@snapshots/gen-2 ro"
        );
        assert_eq!(with_root_subvol("", "@", true), "rootflags=subvol=@ ro");
    }

    #[test]
    fn rejects_unsafe_boot_info() {
        let ok = BootInfo { kernel: "vmlinuz-6.1".into(), initramfs: "initramfs-6.1.img".into(), cmdline: "a b".into() };
        assert!(ok.validate().is_ok());
        assert!(BootInfo { kernel: "../x".into(), ..ok.clone() }.validate().is_err());
        assert!(BootInfo { initramfs: "a b".into(), ..ok.clone() }.validate().is_err());
        assert!(BootInfo { cmdline: "a\nb".into(), ..ok }.validate().is_err());
    }
}

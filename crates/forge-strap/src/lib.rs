//! `forge strap`: install Novara from the live system onto a disk.
//!
//! [`plan`] validates the request and produces the ordered list of steps
//! (pure, no side effects). [`execute`] carries the plan out through a
//! [`Runner`], so every external command can be recorded and checked in tests
//! without touching a disk.

mod exec;

use std::fmt;

pub use exec::{
    Accounts, Cmd, NewUser, Request, Runner, StrapError, SystemRunner, Ui, execute, preflight,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Firmware {
    Uefi,
    Bios,
}

/// Btrfs subvolumes created on the root partition.
pub const SUBVOLUMES: [&str; 4] = ["@", "@snapshots", "@home", "@store"];

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PlanError {
    #[error("`{0}` is not a block device path under /dev")]
    NotADevice(String),
    #[error("root and boot device are the same: {0}")]
    SameDevice(String),
    #[error("a boot partition is required (Limine cannot read btrfs, so /boot is its own FAT32 partition)")]
    BootRequired,
    #[error("`{0}` looks like a whole disk, not a partition")]
    WholeDisk(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    FormatBoot { dev: String, fs: &'static str },
    FormatRoot { dev: String, fs: &'static str },
    CreateSubvolumes(Vec<&'static str>),
    MountTarget,
    CopyRootfs,
    ConfigureTarget,
    InstallLimine(Firmware),
    WriteLimineConf,
    CreateUsers,
    Finish,
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Step::FormatBoot { dev, fs } => write!(f, "format {dev} as {fs} (ERASES the partition)"),
            Step::FormatRoot { dev, fs } => write!(f, "format {dev} as {fs} (ERASES the partition)"),
            Step::CreateSubvolumes(v) => write!(f, "create btrfs subvolumes: {}", v.join(", ")),
            Step::MountTarget => write!(f, "mount the target under the mount point"),
            Step::CopyRootfs => write!(f, "copy the live system to the target (rsync -aHX)"),
            Step::ConfigureTarget => {
                write!(f, "write /etc/fstab and a starter /etc/forge/system.toml")
            }
            Step::InstallLimine(Firmware::Uefi) => {
                write!(f, "install Limine (UEFI) and add a firmware boot entry")
            }
            Step::InstallLimine(Firmware::Bios) => write!(f, "install Limine (BIOS) to the disk"),
            Step::WriteLimineConf => write!(f, "write limine.conf with the first boot entry"),
            Step::CreateUsers => write!(f, "create the user and set passwords you entered"),
            Step::Finish => write!(f, "sync and unmount the target"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub root_dev: String,
    pub boot_dev: String,
    pub firmware: Firmware,
    pub steps: Vec<Step>,
}

fn check_dev(dev: &str) -> Result<(), PlanError> {
    let ok = dev.starts_with("/dev/") && dev.len() > "/dev/".len() && !dev.contains("..");
    if !ok {
        return Err(PlanError::NotADevice(dev.into()));
    }
    // Reject obvious whole-disk names: sda, vda, hda, nvme0n1, mmcblk0.
    let name = &dev["/dev/".len()..];
    let whole = matches!(name.len(), 3 if ["sd", "vd", "hd"].iter().any(|p| name.starts_with(p)))
        || (name.starts_with("nvme") && !name.contains('p'))
        || (name.starts_with("mmcblk") && !name.contains('p'));
    if whole {
        return Err(PlanError::WholeDisk(dev.into()));
    }
    Ok(())
}

/// Split a partition device into its disk and partition number:
/// `/dev/sda1` -> (`/dev/sda`, 1), `/dev/nvme0n1p2` -> (`/dev/nvme0n1`, 2).
pub fn split_partition(dev: &str) -> Option<(String, u32)> {
    let digits = dev.bytes().rev().take_while(u8::is_ascii_digit).count();
    if digits == 0 || digits == dev.len() {
        return None;
    }
    let (head, num) = dev.split_at(dev.len() - digits);
    let disk = match head.strip_suffix('p') {
        Some(d) if d.bytes().last().is_some_and(|b| b.is_ascii_digit()) => d,
        _ => head,
    };
    Some((disk.to_string(), num.parse().ok()?))
}

/// Build the ordered install plan, or explain why the request is invalid.
pub fn plan(firmware: Firmware, root_dev: &str, boot_dev: Option<&str>) -> Result<Plan, PlanError> {
    check_dev(root_dev)?;
    let boot = boot_dev.ok_or(PlanError::BootRequired)?;
    check_dev(boot)?;
    if boot == root_dev {
        return Err(PlanError::SameDevice(boot.into()));
    }

    let steps = vec![
        Step::FormatBoot { dev: boot.into(), fs: "FAT32" },
        Step::FormatRoot { dev: root_dev.into(), fs: "btrfs" },
        Step::CreateSubvolumes(SUBVOLUMES.to_vec()),
        Step::MountTarget,
        Step::CopyRootfs,
        Step::ConfigureTarget,
        Step::InstallLimine(firmware),
        Step::WriteLimineConf,
        Step::CreateUsers,
        Step::Finish,
    ];
    Ok(Plan { root_dev: root_dev.into(), boot_dev: boot.into(), firmware, steps })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_formats_boot_and_creates_all_subvolumes() {
        for fw in [Firmware::Uefi, Firmware::Bios] {
            let p = plan(fw, "/dev/sda2", Some("/dev/sda1")).unwrap();
            assert!(matches!(p.steps[0], Step::FormatBoot { .. }));
            assert!(p.steps.contains(&Step::CreateSubvolumes(SUBVOLUMES.to_vec())));
            assert_eq!(p.steps.last(), Some(&Step::Finish));
        }
    }

    #[test]
    fn users_are_created_after_the_copy() {
        let p = plan(Firmware::Bios, "/dev/sda2", Some("/dev/sda1")).unwrap();
        let copy = p.steps.iter().position(|s| *s == Step::CopyRootfs).unwrap();
        let users = p.steps.iter().position(|s| *s == Step::CreateUsers).unwrap();
        assert!(users > copy);
    }

    #[test]
    fn boot_partition_is_always_required() {
        for fw in [Firmware::Uefi, Firmware::Bios] {
            assert_eq!(plan(fw, "/dev/sda2", None), Err(PlanError::BootRequired));
        }
    }

    #[test]
    fn rejects_same_device_and_non_devices() {
        assert!(matches!(
            plan(Firmware::Uefi, "/dev/sda2", Some("/dev/sda2")),
            Err(PlanError::SameDevice(_))
        ));
        assert!(matches!(
            plan(Firmware::Bios, "sda2", Some("/dev/sda1")),
            Err(PlanError::NotADevice(_))
        ));
        assert!(matches!(
            plan(Firmware::Bios, "/dev/../etc", Some("/dev/sda1")),
            Err(PlanError::NotADevice(_))
        ));
    }

    #[test]
    fn rejects_whole_disks() {
        for d in ["/dev/sda", "/dev/vdb", "/dev/nvme0n1", "/dev/mmcblk0"] {
            assert!(
                matches!(plan(Firmware::Bios, d, Some("/dev/sda1")), Err(PlanError::WholeDisk(_))),
                "{d}"
            );
        }
        assert!(plan(Firmware::Bios, "/dev/nvme0n1p2", Some("/dev/nvme0n1p1")).is_ok());
        assert!(plan(Firmware::Bios, "/dev/mmcblk0p2", Some("/dev/mmcblk0p1")).is_ok());
    }

    #[test]
    fn splits_partitions_into_disk_and_number() {
        assert_eq!(split_partition("/dev/sda1"), Some(("/dev/sda".into(), 1)));
        assert_eq!(split_partition("/dev/vdb12"), Some(("/dev/vdb".into(), 12)));
        assert_eq!(split_partition("/dev/nvme0n1p2"), Some(("/dev/nvme0n1".into(), 2)));
        assert_eq!(split_partition("/dev/mmcblk0p1"), Some(("/dev/mmcblk0".into(), 1)));
        assert_eq!(split_partition("/dev/loop0p1"), Some(("/dev/loop0".into(), 1)));
        assert_eq!(split_partition("/dev/sda"), None);
        assert_eq!(split_partition("/dev/"), None);
    }
}

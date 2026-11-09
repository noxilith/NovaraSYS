//! The files that make a tree a live system: the initramfs `/init`, the live root's
//! `/etc`, and the Limine configuration.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;

use crate::{IsoError, io_ctx};

/// Where the squashfs sits on the medium (also what `/init` looks for).
pub const SQUASHFS_PATH: &str = "live/rootfs.squashfs";

/// Applets the initramfs needs, linked to the BusyBox binary.
pub const INITRAMFS_APPLETS: &[&str] =
    &["sh", "mount", "umount", "mkdir", "sleep", "switch_root", "echo", "ls", "cat", "dmesg", "losetup", "insmod", "modprobe"];

/// The initramfs `/init`: find the medium that holds the squashfs, stack a writable
/// tmpfs over it with overlayfs, and hand over to the live system's init.
pub fn init_script(label: &str) -> String {
    format!(
        r#"#!/bin/sh
# NovaraSYS live initramfs: find the medium, mount the system, switch to it.
export PATH=/bin
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
echo "novara: looking for the live medium ({label})"

rescue() {{
    echo "novara: $1"
    echo "novara: dropping to a shell; exit to retry"
    sh
}}

find_medium() {{
    for dev in /dev/sr* /dev/vd* /dev/sd* /dev/nvme*n* /dev/mmcblk* /dev/loop*; do
        [ -b "$dev" ] || continue
        mount -t iso9660 -o ro "$dev" /medium 2>/dev/null || continue
        if [ -f "/medium/{squashfs}" ]; then
            echo "novara: found it on $dev"
            return 0
        fi
        umount /medium
    done
    return 1
}}

tries=0
until find_medium; do
    tries=$((tries + 1))
    if [ "$tries" -ge 30 ]; then
        rescue "no medium with {squashfs} was found"
        tries=0
    fi
    sleep 1
done

mount -t squashfs -o ro,loop "/medium/{squashfs}" /ro || rescue "cannot mount the system image"
mount -t tmpfs -o mode=0755 tmpfs /rw
mkdir -p /rw/upper /rw/work
mount -t overlay overlay -o lowerdir=/ro,upperdir=/rw/upper,workdir=/rw/work /newroot \
    || rescue "cannot assemble the writable system"

mkdir -p /newroot/medium /newroot/dev /newroot/proc /newroot/sys
mount --move /medium /newroot/medium
mount --move /dev /newroot/dev
mount --move /proc /newroot/proc
mount --move /sys /newroot/sys
# `init=` on the kernel command line picks the first process (the rescue entry uses init=/bin/sh).
init=/sbin/init
for arg in $(cat /proc/cmdline); do
    case "$arg" in init=*) init=${{arg#init=}} ;; esac
done
exec switch_root /newroot "$init"
"#,
        squashfs = SQUASHFS_PATH
    )
}

/// Populate an initramfs tree: BusyBox, its applet links and `/init`.
pub fn write_initramfs_tree(root: &Path, busybox: &Path, label: &str) -> Result<(), IsoError> {
    for d in ["bin", "dev", "proc", "sys", "medium", "ro", "rw", "newroot", "tmp"] {
        fs::create_dir_all(root.join(d)).map_err(io_ctx(format!("creating /{d}")))?;
    }
    let bb = root.join("bin/busybox");
    fs::copy(busybox, &bb).map_err(io_ctx(format!("copying {}", busybox.display())))?;
    fs::set_permissions(&bb, fs::Permissions::from_mode(0o755)).map_err(io_ctx("setting permissions"))?;
    for a in INITRAMFS_APPLETS {
        symlink("busybox", root.join("bin").join(a)).map_err(io_ctx(format!("linking {a}")))?;
    }
    let init = root.join("init");
    fs::write(&init, init_script(label)).map_err(io_ctx("writing /init"))?;
    fs::set_permissions(&init, fs::Permissions::from_mode(0o755)).map_err(io_ctx("setting permissions"))?;
    Ok(())
}

/// The live root's `/etc` and mount points, written into the merged package tree.
pub fn write_live_root(root: &Path, hostname: &str, password_hash: &str) -> Result<(), IsoError> {
    for d in ["dev", "proc", "sys", "run", "tmp", "mnt", "medium", "root", "home", "var/log", "etc/init.d", "sbin", "bin"] {
        fs::create_dir_all(root.join(d)).map_err(io_ctx(format!("creating /{d}")))?;
    }
    fs::set_permissions(root.join("tmp"), fs::Permissions::from_mode(0o1777)).map_err(io_ctx("setting permissions"))?;
    fs::set_permissions(root.join("root"), fs::Permissions::from_mode(0o700)).map_err(io_ctx("setting permissions"))?;

    let put = |path: &str, mode: u32, text: String| -> Result<(), IsoError> {
        let p = root.join(path);
        fs::write(&p, text).map_err(io_ctx(format!("writing /{path}")))?;
        fs::set_permissions(&p, fs::Permissions::from_mode(mode)).map_err(io_ctx("setting permissions"))
    };
    // BusyBox mount reads the mount table from here.
    let _ = fs::remove_file(root.join("etc/mtab"));
    symlink("/proc/self/mounts", root.join("etc/mtab")).map_err(io_ctx("linking /etc/mtab"))?;
    put("etc/hostname", 0o644, format!("{hostname}\n"))?;
    put("etc/passwd", 0o644, "root:x:0:0:root:/root:/bin/sh\n".into())?;
    put("etc/shadow", 0o600, format!("root:{password_hash}:20000:0:99999:7:::\n"))?;
    put("etc/group", 0o644, "root:x:0:\n".into())?;
    put("etc/shells", 0o644, "/bin/sh\n".into())?;
    put(
        "etc/os-release",
        0o644,
        "NAME=\"NovaraSYS\"\nID=novara\nPRETTY_NAME=\"NovaraSYS (live)\"\nVARIANT=\"live\"\n".into(),
    )?;
    put(
        "etc/issue",
        0o644,
        "\nNovaraSYS live system\nLog in as root (password: root), partition the disk with cfdisk,\nthen run: forge strap <root-partition> <boot-partition>\n\n".into(),
    )?;
    put(
        "etc/profile",
        0o644,
        "# Packages installed with forge are linked into this profile; they win over the base system.\nP=/forge/profiles/system/current\nexport PATH=$P/usr/sbin:$P/usr/bin:$P/sbin:$P/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin\nexport PS1='\\u@\\h:\\w\\$ '\nfor f in /etc/profile.d/*.sh; do [ -r \"$f\" ] && . \"$f\"; done\n".into(),
    )?;
    if root.join("sbin/openrc-init").exists() {
        write_openrc(root)?;
        return Ok(());
    }
    // No OpenRC in the image: BusyBox init with a small rcS keeps it bootable.
    put(
        "etc/inittab",
        0o644,
        "::sysinit:/etc/init.d/rcS\n\
         tty1::respawn:/sbin/getty 38400 tty1\n\
         ttyS0::respawn:/sbin/getty -L 115200 ttyS0 vt100\n\
         ::ctrlaltdel:/sbin/reboot\n\
         ::shutdown:/bin/umount -a -r\n"
            .into(),
    )?;
    put(
        "etc/init.d/rcS",
        0o755,
        // Runs on the live system and on an installed one (where nothing else has mounted the
        // pseudo filesystems), so every mount is conditional. `mount -a` brings up fstab: the
        // btrfs subvolumes of an installed system (the root is already mounted by the kernel).
        "#!/bin/sh\n\
         mountpoint -q /proc || mount -t proc proc /proc\n\
         mountpoint -q /sys || mount -t sysfs sysfs /sys\n\
         mountpoint -q /dev || mount -t devtmpfs devtmpfs /dev\n\
         mkdir -p /dev/pts /dev/shm\n\
         mountpoint -q /dev/pts || mount -t devpts devpts /dev/pts\n\
         mountpoint -q /dev/shm || mount -t tmpfs tmpfs /dev/shm\n\
         mountpoint -q /run || mount -t tmpfs tmpfs /run\n\
         mountpoint -q /tmp || mount -t tmpfs tmpfs /tmp\n\
         chmod 1777 /tmp\n\
         mount -a 2>/dev/null || true\n\
         hostname -F /etc/hostname\n"
            .into(),
    )?;
    // The overlay's /bin may only exist if a package provided it; /sbin/init is BusyBox's.
    Ok(())
}

/// Runlevels of the live system (and, copied by `forge strap`, of an installed one). Services are
/// symlinks `/etc/runlevels/<level>/<service>` -> `/etc/init.d/<service>`: what `forge apply` edits.
const RUNLEVELS: &[(&str, &[&str])] = &[
    ("sysinit", &["devfs", "dmesg", "sysfs"]),
    ("boot", &["procfs", "coldplug", "root", "hostname", "localmount", "bootmisc", "loopback"]),
    ("default", &["network", "agetty.tty1", "agetty.ttyS0", "local"]),
    ("shutdown", &["killprocs", "mount-ro"]),
];

/// Make the tree boot with OpenRC: `/sbin/init` is `openrc-init`, the runlevels are the ones above
/// (the package's defaults include fsck, hwclock and swap, which do not suit a live system), terminals
/// are `agetty` services, and `reboot`/`poweroff`/`halt` go through `openrc-shutdown`
/// (BusyBox's would signal PID 1 in a way openrc-init does not understand).
fn write_openrc(root: &Path) -> Result<(), IsoError> {
    let rl = root.join("etc/runlevels");
    let _ = fs::remove_dir_all(&rl);
    for (level, services) in RUNLEVELS {
        let dir = rl.join(level);
        fs::create_dir_all(&dir).map_err(io_ctx(format!("creating {}", dir.display())))?;
        for s in *services {
            symlink(format!("/etc/init.d/{s}"), dir.join(s)).map_err(io_ctx(format!("enabling {s}")))?;
        }
    }
    // One agetty service per terminal, from the stock template.
    for (port, baud, term, opts) in [("tty1", "38400", "linux", ""), ("ttyS0", "115200", "vt100", "-L")] {
        let svc = root.join(format!("etc/init.d/agetty.{port}"));
        let _ = fs::remove_file(&svc);
        symlink("agetty", &svc).map_err(io_ctx(format!("linking agetty.{port}")))?;
        let conf = root.join(format!("etc/conf.d/agetty.{port}"));
        fs::create_dir_all(conf.parent().expect("has parent")).map_err(io_ctx("creating /etc/conf.d"))?;
        fs::write(&conf, format!("baud=\"{baud}\"\nterm_type=\"{term}\"\nagetty_options=\"{opts}\"\n")).map_err(io_ctx("writing agetty settings"))?;
    }
    write_networking(root)?;
    let _ = fs::remove_file(root.join("etc/inittab"));
    let init = root.join("sbin/init");
    let _ = fs::remove_file(&init);
    symlink("openrc-init", &init).map_err(io_ctx("linking /sbin/init"))?;
    for (name, flag) in [("reboot", "-r"), ("halt", "-H")] {
        let p = root.join("sbin").join(name);
        let _ = fs::remove_file(&p);
        fs::write(&p, format!("#!/bin/sh\nexec /sbin/openrc-shutdown {flag} now\n")).map_err(io_ctx(format!("writing /sbin/{name}")))?;
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).map_err(io_ctx("setting permissions"))?;
    }
    // `shutdown` powers the machine off (there is no `poweroff`: BusyBox's would signal PID 1 the
    // wrong way, so its link is removed rather than left lying around).
    let _ = fs::remove_file(root.join("sbin/poweroff"));
    let p = root.join("sbin/shutdown");
    let _ = fs::remove_file(&p);
    fs::write(&p, SHUTDOWN_SCRIPT).map_err(io_ctx("writing /sbin/shutdown"))?;
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).map_err(io_ctx("setting permissions"))?;
    Ok(())
}

pub fn limine_conf(cmdline: &str) -> String {
    let entry = |name: &str, extra: &str| {
        format!(
            "/{name}\n    protocol: linux\n    path: boot():/boot/vmlinuz\n    cmdline: {cmdline}{extra}\n    module_path: boot():/boot/initramfs.img\n"
        )
    };
    // The rescue entry skips the init system entirely: a shell on the console, to look around
    // if OpenRC does not come up.
    format!(
        "timeout: 5\n\n{}\n{}",
        entry("NovaraSYS live", ""),
        entry("NovaraSYS live (rescue shell, no init system)", " init=/bin/sh")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_looks_for_the_squashfs_and_hands_over() {
        let s = init_script("NOVARA_LIVE");
        assert!(s.starts_with("#!/bin/sh"));
        assert!(s.contains("/medium/live/rootfs.squashfs"));
        assert!(s.contains("mount -t overlay"));
        assert!(s.trim_end().ends_with("exec switch_root /newroot \"$init\""));
        assert!(s.contains("init=/sbin/init"), "the default first process");
        assert!(s.contains("init=*) init=${arg#init=}"), "init= from the command line is honoured");
    }

    #[test]
    fn live_root_has_root_login_and_a_tmp_that_everyone_can_use() {
        let t = tempfile::tempdir().unwrap();
        write_live_root(t.path(), "novara-live", "$6$salt$hash").unwrap();
        assert!(fs::read_to_string(t.path().join("etc/shadow")).unwrap().starts_with("root:$6$salt$hash:"));
        assert_eq!(fs::metadata(t.path().join("etc/shadow")).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::metadata(t.path().join("tmp")).unwrap().permissions().mode() & 0o7777, 0o1777);
        assert!(fs::read_to_string(t.path().join("etc/inittab")).unwrap().contains("getty"));
        let profile = fs::read_to_string(t.path().join("etc/profile")).unwrap();
        assert!(profile.contains("P=/forge/profiles/system/current") && profile.contains("$P/usr/bin"), "installed packages are on PATH: {profile}");
        assert!(profile.contains("/etc/profile.d/*.sh"), "forge apply writes its environment there");
        let rcs = fs::read_to_string(t.path().join("etc/init.d/rcS")).unwrap();
        assert!(rcs.contains("mountpoint -q /proc || mount -t proc proc /proc"), "installed systems need /proc too");
        assert!(rcs.contains("mount -a"));
        assert_eq!(fs::read_link(t.path().join("etc/mtab")).unwrap(), Path::new("/proc/self/mounts"));
    }

    #[test]
    fn initramfs_tree_links_applets_to_busybox() {
        let t = tempfile::tempdir().unwrap();
        let bb = t.path().join("bb");
        fs::write(&bb, "ELF").unwrap();
        let root = t.path().join("rd");
        write_initramfs_tree(&root, &bb, "L").unwrap();
        assert_eq!(fs::read_link(root.join("bin/switch_root")).unwrap(), Path::new("busybox"));
        assert!(root.join("init").exists());
        assert_eq!(fs::metadata(root.join("init")).unwrap().permissions().mode() & 0o111, 0o111);
    }

    #[test]
    fn limine_entry_names_kernel_initramfs_and_cmdline() {
        let c = limine_conf("console=ttyS0");
        assert!(c.contains("path: boot():/boot/vmlinuz"));
        assert!(c.contains("module_path: boot():/boot/initramfs.img"));
        assert!(c.contains("cmdline: console=ttyS0\n"));
        assert!(c.contains("cmdline: console=ttyS0 init=/bin/sh\n"), "a rescue entry without the init system: {c}");
        assert_eq!(c.matches("module_path").count(), 2);
    }
}

#[cfg(test)]
mod openrc_tests {
    use super::*;

    #[test]
    fn with_openrc_the_tree_boots_through_it() {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir_all(t.path().join("sbin")).unwrap();
        fs::write(t.path().join("sbin/openrc-init"), "ELF").unwrap();
        // What the busybox and openrc packages leave behind.
        symlink("../bin/busybox", t.path().join("sbin/init")).unwrap();
        symlink("../bin/busybox", t.path().join("sbin/reboot")).unwrap();
        symlink("../bin/busybox", t.path().join("sbin/poweroff")).unwrap();
        fs::create_dir_all(t.path().join("etc/runlevels/boot")).unwrap();
        symlink("/etc/init.d/fsck", t.path().join("etc/runlevels/boot/fsck")).unwrap();
        write_live_root(t.path(), "novara-live", "$6$s$h").unwrap();

        assert_eq!(fs::read_link(t.path().join("sbin/init")).unwrap(), Path::new("openrc-init"));
        assert!(!t.path().join("etc/inittab").exists());
        assert!(!t.path().join("etc/init.d/rcS").exists(), "no BusyBox rcS when OpenRC runs the system");
        // The package's default runlevels are replaced, not extended.
        assert!(fs::symlink_metadata(t.path().join("etc/runlevels/boot/fsck")).is_err());
        assert_eq!(
            fs::read_link(t.path().join("etc/runlevels/boot/localmount")).unwrap(),
            Path::new("/etc/init.d/localmount")
        );
        assert!(fs::symlink_metadata(t.path().join("etc/runlevels/default/agetty.tty1")).is_ok());
        assert!(fs::symlink_metadata(t.path().join("etc/runlevels/default/network")).is_ok(), "networking starts by default");
        assert!(fs::symlink_metadata(t.path().join("etc/runlevels/boot/coldplug")).is_ok(), "drivers are loaded for the hardware found");
        let svc = fs::read_to_string(t.path().join("etc/init.d/network")).unwrap();
        assert!(svc.starts_with("#!/sbin/openrc-run") && svc.contains("netwatch"), "{svc}");
        let watch = fs::read_to_string(t.path().join("usr/libexec/novara/netwatch")).unwrap();
        assert!(watch.contains("wpa_supplicant -B") && watch.contains("udhcpc -b -i"), "{watch}");
        assert!(fs::metadata(t.path().join("usr/bin/wifi")).unwrap().permissions().mode() & 0o111 != 0);
        assert!(fs::read_to_string(t.path().join("etc/wpa_supplicant/wpa_supplicant.conf")).unwrap().contains("update_config=1"));
        assert!(fs::metadata(t.path().join("usr/share/udhcpc/default.script")).unwrap().permissions().mode() & 0o111 != 0);
        assert_eq!(fs::read_link(t.path().join("etc/init.d/agetty.tty1")).unwrap(), Path::new("agetty"));
        let conf = fs::read_to_string(t.path().join("etc/conf.d/agetty.ttyS0")).unwrap();
        assert!(conf.contains("baud=\"115200\"") && conf.contains("-L"), "{conf}");
        // BusyBox's reboot would signal PID 1 in a way openrc-init does not handle.
        let reboot = fs::read_to_string(t.path().join("sbin/reboot")).unwrap();
        assert!(reboot.contains("openrc-shutdown -r now"), "{reboot}");
        // `shutdown` replaces `poweroff`, and BusyBox's poweroff link is gone.
        assert!(fs::symlink_metadata(t.path().join("sbin/poweroff")).is_err());
        let sd = fs::read_to_string(t.path().join("sbin/shutdown")).unwrap();
        assert!(sd.contains("openrc-shutdown $flag $when") && sd.contains("flag=-p"), "{sd}");
        assert_eq!(fs::metadata(t.path().join("sbin/shutdown")).unwrap().permissions().mode() & 0o111, 0o111);
    }

    #[test]
    fn without_openrc_busybox_init_is_the_fallback() {
        let t = tempfile::tempdir().unwrap();
        write_live_root(t.path(), "h", "x").unwrap();
        assert!(t.path().join("etc/inittab").exists());
        assert!(t.path().join("etc/init.d/rcS").exists());
        assert!(!t.path().join("etc/runlevels").exists());
    }
}

/// `shutdown [-h|-P|-p|-H|-r] [now|+minutes]`: power off by default; `-r` reboots, `-H` halts.
/// A thin front end for `openrc-shutdown`, which takes the time as a required argument.
const SHUTDOWN_SCRIPT: &str = r#"#!/bin/sh
flag=-p
when=now
for a in "$@"; do
    case "$a" in
        -h|-P|-p) flag=-p ;;
        -H) flag=-H ;;
        -r) flag=-r ;;
        -*) ;;
        *) when=$a ;;
    esac
done
exec /sbin/openrc-shutdown $flag $when
"#;

/// Networking: one OpenRC service (`network`) whose watcher configures every interface as it appears
/// (wired or Wi-Fi), the script `udhcpc` calls to apply a lease, kernel-module autoloading, and the
/// `wifi` helper. Wi-Fi interfaces appear a second or two after their driver loads (firmware loading is
/// asynchronous), which is why this is a watcher and not a one-shot start.
fn write_networking(root: &Path) -> Result<(), IsoError> {
    let put = |path: &str, mode: u32, text: &str| -> Result<(), IsoError> {
        let p = root.join(path);
        fs::create_dir_all(p.parent().expect("has parent")).map_err(io_ctx(format!("creating the directory of /{path}")))?;
        let _ = fs::remove_file(&p);
        fs::write(&p, text).map_err(io_ctx(format!("writing /{path}")))?;
        fs::set_permissions(&p, fs::Permissions::from_mode(mode)).map_err(io_ctx("setting permissions"))
    };
    put("etc/init.d/network", 0o755, NETWORK_SERVICE)?;
    put("etc/conf.d/network", 0o644, NETWORK_CONF)?;
    put("usr/libexec/novara/netwatch", 0o755, NETWATCH)?;
    put("etc/init.d/coldplug", 0o755, COLDPLUG_SERVICE)?;
    put("usr/share/udhcpc/default.script", 0o755, UDHCPC_SCRIPT)?;
    put("usr/bin/wifi", 0o755, WIFI_COMMAND)?;
    put("etc/wpa_supplicant/wpa_supplicant.conf", 0o600, WPA_CONF)
}

const NETWORK_CONF: &str = "# Interfaces to manage, separated by spaces. Empty: every physical interface (wired and wireless).\n\
interfaces=\"\"\n";

const NETWORK_SERVICE: &str = r#"#!/sbin/openrc-run
description="Network: DHCP on wired interfaces, wpa_supplicant and DHCP on Wi-Fi"
command=/usr/libexec/novara/netwatch
command_background=true
pidfile=/run/netwatch.pid

depend() {
	need localmount
	after bootmisc loopback modules-load
	provide net
}

start_pre() {
	# A radio that is soft-blocked stays off until something unblocks it.
	for f in /sys/class/rfkill/rfkill*/soft; do
		[ -w "$f" ] && echo 0 > "$f"
	done
	mkdir -p /run/netwatch
}

stop_post() {
	for p in /run/netwatch/*; do
		[ -f "$p" ] || continue
		kill "$(cat "$p")" 2>/dev/null
		rm -f "$p"
	done
}
"#;

const NETWATCH: &str = r#"#!/bin/sh
# Keeps every network interface configured as it appears, until it is stopped: brings the link up,
# starts wpa_supplicant on wireless ones (when a configuration exists), and runs DHCP on all of them.
[ -r /etc/conf.d/network ] && . /etc/conf.d/network
RUN=/run/netwatch
mkdir -p "$RUN"

alive() { [ -f "$1" ] && kill -0 "$(cat "$1")" 2>/dev/null; }

while :; do
	for dir in /sys/class/net/*; do
		name=${dir##*/}
		[ "$name" = lo ] && continue
		if [ -n "$interfaces" ]; then
			case " $interfaces " in *" $name "*) ;; *) continue ;; esac
		else
			[ -e "$dir/device" ] || continue   # virtual: bridges, tunnels...
		fi
		ip link set "$name" up 2>/dev/null
		if [ -d "$dir/wireless" ] || [ -d "$dir/phy80211" ]; then
			if [ -f /etc/wpa_supplicant/wpa_supplicant.conf ] && ! alive "$RUN/wpa.$name"; then
				wpa_supplicant -B -i "$name" -D nl80211 -c /etc/wpa_supplicant/wpa_supplicant.conf \
					-P "$RUN/wpa.$name" >/dev/null 2>&1
			fi
		fi
		# -b: keep trying in the background; -A 5: retry every 5 seconds (a Wi-Fi link needs a moment).
		alive "$RUN/dhcp.$name" || udhcpc -b -i "$name" -p "$RUN/dhcp.$name" -A 5 \
			-s /usr/share/udhcpc/default.script -x "hostname:$(hostname)" >/dev/null 2>&1
	done
	sleep 3
done
"#;

const COLDPLUG_SERVICE: &str = r#"#!/sbin/openrc-run
description="Loads drivers for the hardware found, and the modules listed in /etc/modules-load.d"

depend() {
	need procfs
	before localmount hostname
	provide modules-load
}

start() {
	ebegin "Loading drivers for detected hardware"
	# Every device offers a modalias; modprobe resolves it to the module that drives it.
	find /sys/devices -name modalias -type f -exec cat {} + 2>/dev/null | sort -u |
		while read -r alias; do
			modprobe -q "$alias" 2>/dev/null
		done
	for file in /usr/lib/modules-load.d/*.conf /etc/modules-load.d/*.conf; do
		[ -f "$file" ] || continue
		while read -r module _; do
			case "$module" in ''|\#*|\;*) continue ;; esac
			modprobe -q "$module" 2>/dev/null
		done < "$file"
	done
	eend 0
}
"#;

const WPA_CONF: &str = "ctrl_interface=/run/wpa_supplicant\n\
ctrl_interface_group=0\n\
update_config=1\n";

const WIFI_COMMAND: &str = r#"#!/bin/sh
# wifi scan | connect SSID [passphrase] | status | disconnect
conf=/etc/wpa_supplicant/wpa_supplicant.conf

find_interface() {
	for d in /sys/class/net/*; do
		if [ -d "$d/wireless" ] || [ -d "$d/phy80211" ]; then echo "${d##*/}"; return; fi
	done
}

iface=$(find_interface)
if [ -z "$iface" ]; then
	echo "wifi: no wireless interface found." >&2
	echo "      Give the drivers a few seconds after boot, then check: dmesg | grep -i -E 'firmware|wlan|iwl|rtw|ath|mt7|brcm'" >&2
	exit 1
fi

case "$1" in
	scan)
		wpa_cli -i "$iface" scan >/dev/null
		sleep 4
		printf 'signal   security            network\n'
		wpa_cli -i "$iface" scan_results | tail -n +2 | sort -t "$(printf '\t')" -k3 -nr |
			awk -F '\t' '{ printf "%4s dBm %-19s %s\n", $3, substr($4, 1, 19), $5 }'
		;;
	connect)
		ssid=$2
		[ -n "$ssid" ] || { echo "usage: wifi connect SSID [passphrase]" >&2; exit 1; }
		pass=$3
		if [ -z "$pass" ]; then
			printf 'Passphrase for %s (empty for an open network): ' "$ssid" >&2
			stty -echo 2>/dev/null; read -r pass; stty echo 2>/dev/null; echo >&2
		fi
		if [ -z "$pass" ]; then
			printf 'network={\n\tssid="%s"\n\tkey_mgmt=NONE\n}\n' "$ssid" >> "$conf"
		else
			wpa_passphrase "$ssid" "$pass" | grep -v '#psk' >> "$conf" || exit 1
		fi
		wpa_cli -i "$iface" reconfigure >/dev/null
		printf 'Connecting to %s ' "$ssid"
		n=0
		while [ "$n" -lt 25 ]; do
			if wpa_cli -i "$iface" status | grep -q 'wpa_state=COMPLETED'; then
				echo "associated."
				sleep 3
				ip -4 addr show "$iface" | grep inet || echo "(no IP address yet: DHCP is still trying)"
				exit 0
			fi
			printf '.'; sleep 1; n=$((n + 1))
		done
		echo " not connected. Check the passphrase, and that the network is in range (wifi scan)."
		exit 1
		;;
	status)
		wpa_cli -i "$iface" status
		ip -4 addr show "$iface" | grep inet
		;;
	disconnect)
		wpa_cli -i "$iface" disconnect
		;;
	*)
		echo "usage: wifi scan | connect SSID [passphrase] | status | disconnect" >&2
		exit 1
		;;
esac
"#;

const UDHCPC_SCRIPT: &str = r#"#!/bin/sh
# Called by udhcpc: $1 is deconfig, bound, renew, nak or leasefail; the lease arrives in the
# environment (interface, ip, mask, broadcast, router, dns, domain).
case "$1" in
	deconfig)
		ip -4 addr flush dev "$interface"
		ip link set "$interface" up
		;;
	bound|renew)
		ip -4 addr replace "$ip/${mask:-24}" ${broadcast:+broadcast "$broadcast"} dev "$interface"
		if [ -n "$router" ]; then
			metric=0
			for gw in $router; do
				metric=$((metric + 1))
				ip -4 route replace default via "$gw" dev "$interface" metric "$metric"
			done
		fi
		if [ -n "$dns" ]; then
			tmp=/etc/resolv.conf.$$
			{
				[ -n "$domain" ] && echo "search $domain"
				for server in $dns; do echo "nameserver $server"; done
			} > "$tmp" && mv "$tmp" /etc/resolv.conf
		fi
		;;
esac
exit 0
"#;

//! A minimal, deterministic `newc` cpio writer (the format the kernel unpacks as an
//! initramfs). No host `cpio` is needed. Entries are sorted, times are zero and
//! inode numbers are sequential, so the same tree always gives the same bytes.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::Path;

const S_IFMT: u32 = 0o170000;

fn pad4<W: Write>(w: &mut W, len: usize) -> io::Result<()> {
    w.write_all(&[0u8; 3][..(4 - len % 4) % 4])
}

fn entry<W: Write>(w: &mut W, ino: u32, mode: u32, nlink: u32, rdev: (u32, u32), name: &str, data: &[u8]) -> io::Result<()> {
    let namesz = name.len() + 1;
    write!(
        w,
        "070701{ino:08X}{mode:08X}{:08X}{:08X}{nlink:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{namesz:08X}{:08X}",
        0, 0, 0, data.len(), 0, 0, rdev.0, rdev.1, 0
    )?;
    w.write_all(name.as_bytes())?;
    w.write_all(&[0])?;
    pad4(w, 110 + namesz)?;
    w.write_all(data)?;
    pad4(w, data.len())
}

/// Write the contents of `root` as a cpio archive (the root itself is not an entry).
/// Device nodes to put in `/dev` of the archive: (name, mode incl. file type, (major, minor)).
/// They are written as archive entries because a normal user cannot `mknod` them on disk,
/// and an initramfs needs `/dev/console` before `/init` runs.
pub type Device = (&'static str, u32, (u32, u32));

pub fn write_tree<W: Write>(w: &mut W, root: &Path, devices: &[Device]) -> io::Result<()> {
    let mut ino = 1u32;
    walk(w, root, "", &mut ino, devices)?;
    entry(w, 0, 0, 1, (0, 0), "TRAILER!!!", &[])
}

fn walk<W: Write>(w: &mut W, dir: &Path, prefix: &str, ino: &mut u32, devices: &[Device]) -> io::Result<()> {
    let mut names: Vec<_> = fs::read_dir(dir)?.collect::<Result<Vec<_>, _>>()?;
    names.sort_by_key(|e| e.file_name());
    for e in names {
        let name = format!("{prefix}{}", e.file_name().to_string_lossy());
        let meta = fs::symlink_metadata(e.path())?;
        let mode = meta.mode();
        let ft = meta.file_type();
        *ino += 1;
        if ft.is_dir() {
            entry(w, *ino, mode, 2, (0, 0), &name, &[])?;
            walk(w, &e.path(), &format!("{name}/"), ino, devices)?;
            if name == "dev" {
                for (dev, mode, rdev) in devices {
                    *ino += 1;
                    entry(w, *ino, *mode, 1, *rdev, &format!("dev/{dev}"), &[])?;
                }
            }
        } else if ft.is_symlink() {
            let target = fs::read_link(e.path())?;
            entry(w, *ino, mode, 1, (0, 0), &name, target.as_os_str().as_encoded_bytes())?;
        } else if ft.is_file() {
            entry(w, *ino, mode, 1, (0, 0), &name, &fs::read(e.path())?)?;
        } else if ft.is_block_device() || ft.is_char_device() {
            let r = meta.rdev();
            entry(w, *ino, mode, 1, (((r >> 8) & 0xfff) as u32, (r & 0xff) as u32), &name, &[])?;
        } else {
            return Err(io::Error::other(format!("{}: unsupported file type (mode {:o})", e.path().display(), mode & S_IFMT)));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse our own output back: (name, mode, data).
    fn parse(mut b: &[u8]) -> Vec<(String, u32, Vec<u8>)> {
        let mut out = vec![];
        loop {
            assert_eq!(&b[..6], b"070701");
            let f = |i: usize| u32::from_str_radix(std::str::from_utf8(&b[6 + i * 8..14 + i * 8]).unwrap(), 16).unwrap() as usize;
            let (mode, size, namesz) = (f(1), f(6), f(11));
            let name = String::from_utf8(b[110..110 + namesz - 1].to_vec()).unwrap();
            let data_at = (110 + namesz).next_multiple_of(4);
            let data = b[data_at..data_at + size].to_vec();
            b = &b[(data_at + size).next_multiple_of(4)..];
            if name == "TRAILER!!!" {
                assert!(b.is_empty());
                return out;
            }
            out.push((name, mode as u32, data));
        }
    }

    #[test]
    fn round_trips_files_dirs_and_symlinks_in_order() {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir_all(t.path().join("bin")).unwrap();
        fs::write(t.path().join("bin/a"), "hello").unwrap();
        fs::write(t.path().join("init"), "#!/bin/sh\n").unwrap(); // 10 bytes: needs padding
        std::os::unix::fs::symlink("a", t.path().join("bin/b")).unwrap();
        let mut buf = vec![];
        write_tree(&mut buf, t.path(), &[]).unwrap();
        assert_eq!(buf.len() % 4, 0);
        let got = parse(&buf);
        let names: Vec<_> = got.iter().map(|e| e.0.as_str()).collect();
        assert_eq!(names, ["bin", "bin/a", "bin/b", "init"]);
        assert_eq!(got[1].2, b"hello");
        assert_eq!(got[2].2, b"a");
        assert_eq!(got[2].1 & S_IFMT, 0o120000);
        assert_eq!(got[3].2, b"#!/bin/sh\n");
    }

    #[test]
    fn the_same_tree_gives_the_same_bytes() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join("x"), "1").unwrap();
        fs::write(t.path().join("y"), "2").unwrap();
        let (mut a, mut b) = (vec![], vec![]);
        write_tree(&mut a, t.path(), &[]).unwrap();
        write_tree(&mut b, t.path(), &[]).unwrap();
        assert_eq!(a, b);
    }
}

#[cfg(test)]
mod device_tests {
    use super::*;

    #[test]
    fn device_nodes_follow_their_directory_entry() {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir(t.path().join("dev")).unwrap();
        fs::write(t.path().join("init"), "x").unwrap();
        let mut buf = vec![];
        write_tree(&mut buf, t.path(), &[("console", 0o020600, (5, 1)), ("null", 0o020666, (1, 3))]).unwrap();
        let text = String::from_utf8_lossy(&buf);
        let dev = text.find("dev\0").unwrap();
        let console = text.find("dev/console\0").unwrap();
        assert!(dev < console && console < text.find("dev/null\0").unwrap());
        // The header of /dev/console carries the character-device mode and its numbers.
        let hdr = &buf[console - 110..console];
        assert_eq!(&hdr[14..22], b"00002180"); // mode 020600
        assert_eq!(&hdr[78..86], b"00000005"); // rdevmajor
        assert_eq!(&hdr[86..94], b"00000001"); // rdevminor
    }
}

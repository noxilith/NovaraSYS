//! Unpacking source tarballs (`.tar`, `.tar.gz`/`.tgz`, `.tar.xz`/`.txz`,
//! `.tar.zst`). All decompressors are pure Rust.

use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read};
use std::path::{Path, PathBuf};

use crate::{BuildError, io_ctx};

fn open_tar_stream(archive: &Path, scratch: &Path) -> Result<Box<dyn Read>, BuildError> {
    let name = archive.file_name().and_then(|n| n.to_str()).unwrap_or("").to_ascii_lowercase();
    let open = || File::open(archive).map_err(io_ctx(format!("opening {}", archive.display())));
    if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        return Ok(Box::new(flate2::read::GzDecoder::new(BufReader::new(open()?))));
    }
    if name.ends_with(".tar.zst") || name.ends_with(".tar.zstd") {
        let dec = ruzstd::decoding::StreamingDecoder::new(open()?)
            .map_err(|e| BuildError::UnsupportedArchive(format!("{}: {e}", archive.display())))?;
        return Ok(Box::new(dec));
    }
    if name.ends_with(".tar.xz") || name.ends_with(".txz") {
        // The xz decoder writes to a sink, so decompress to a scratch file first.
        let tar_path = scratch.join(".decompressed.tar");
        let mut input = BufReader::new(open()?);
        let mut output = BufWriter::new(
            File::create(&tar_path).map_err(io_ctx(format!("creating {}", tar_path.display())))?,
        );
        lzma_rs::xz_decompress(&mut input, &mut output)
            .map_err(|e| BuildError::UnsupportedArchive(format!("{}: {e}", archive.display())))?;
        drop(output);
        let f = File::open(&tar_path).map_err(io_ctx(format!("opening {}", tar_path.display())))?;
        let _ = fs::remove_file(&tar_path); // the open handle keeps the data readable
        return Ok(Box::new(f));
    }
    if name.ends_with(".tar.bz2") || name.ends_with(".tbz2") || name.ends_with(".tbz") {
        // MultiBzDecoder: a .bz2 may be several concatenated streams, as pbzip2 writes them.
        return Ok(Box::new(bzip2::read::MultiBzDecoder::new(BufReader::new(open()?))));
    }
    if name.ends_with(".tar") {
        return Ok(Box::new(BufReader::new(open()?)));
    }
    Err(BuildError::UnsupportedArchive(format!(
        "{} (supported: .tar, .tar.gz, .tgz, .tar.bz2, .tar.xz, .txz, .tar.zst)",
        archive.display()
    )))
}

/// Unpack `archive` into the new directory `dest` and return the directory
/// that holds the sources: the single top-level directory most tarballs have,
/// or `dest` itself if there are several entries at the top.
pub fn extract(archive: &Path, dest: &Path) -> Result<PathBuf, BuildError> {
    fs::create_dir_all(dest).map_err(io_ctx(format!("creating {}", dest.display())))?;
    let stream = open_tar_stream(archive, dest)?;
    let mut tar = tar::Archive::new(stream);
    tar.set_preserve_permissions(true);
    tar.set_unpack_xattrs(false);
    tar.set_overwrite(false);

    // Like GNU tar, a regular file that appears twice is replaced by the later entry
    // (btrfs-progs-6.11 ships one). Only a regular file this extraction itself wrote is
    // ever replaced, and only if its directory really is inside `dest`.
    let mut written = std::collections::HashSet::<PathBuf>::new();
    let root = fs::canonicalize(dest).map_err(io_ctx("resolving the source directory"))?;

    // Reject anything that would land outside `dest` instead of silently skipping it.
    let entries = tar.entries().map_err(io_ctx(format!("reading {}", archive.display())))?;
    for entry in entries {
        let mut entry = entry.map_err(io_ctx(format!("reading {}", archive.display())))?;
        let path = entry.path().map_err(io_ctx("reading an entry name"))?.into_owned();
        let escapes = path.is_absolute()
            || path.components().any(|c| matches!(c, std::path::Component::ParentDir));
        if escapes {
            return Err(BuildError::UnsafeSource(format!("entry `{}` points outside the source directory", path.display())));
        }
        if entry.header().entry_type().is_file() && !written.insert(path.clone()) {
            let target = dest.join(&path);
            let inside = target.parent().and_then(|p| fs::canonicalize(p).ok()).is_some_and(|p| p.starts_with(&root));
            let regular = fs::symlink_metadata(&target).is_ok_and(|m| m.file_type().is_file());
            if inside && regular {
                fs::remove_file(&target).map_err(io_ctx(format!("replacing {}", path.display())))?;
            }
        }
        if !entry.unpack_in(dest).map_err(io_ctx(format!("unpacking {}", path.display())))? {
            return Err(BuildError::UnsafeSource(format!("entry `{}` was refused", path.display())));
        }
    }

    let mut top: Vec<PathBuf> = fs::read_dir(dest)
        .map_err(io_ctx(format!("reading {}", dest.display())))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    top.sort();
    Ok(match top.as_slice() {
        [only] if only.is_dir() => only.clone(),
        _ => dest.to_path_buf(),
    })
}

#[cfg(test)]
mod bz2_fixture;

#[cfg(test)]
mod tests {
    use super::bz2_fixture::SAMPLE_TAR_BZ2;
    use super::*;
    use std::io::Write;

    /// A tar with `proj-1.0/configure` and `proj-1.0/src/main.c`.
    fn sample_tar() -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (path, content) in [("proj-1.0/configure", "#!/bin/sh\n"), ("proj-1.0/src/main.c", "int main(){}\n")] {
            let mut h = tar::Header::new_gnu();
            h.set_size(content.len() as u64);
            h.set_mode(0o755);
            h.set_cksum();
            b.append_data(&mut h, path, content.as_bytes()).unwrap();
        }
        b.into_inner().unwrap()
    }

    fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, bytes).unwrap();
        p
    }

    fn check(root: &Path) {
        assert_eq!(fs::read_to_string(root.join("configure")).unwrap(), "#!/bin/sh\n");
        assert_eq!(fs::read_to_string(root.join("src/main.c")).unwrap(), "int main(){}\n");
    }

    #[test]
    fn unpacks_plain_gzip_zstd_and_xz_tarballs() {
        let d = tempfile::tempdir().unwrap();
        let tar = sample_tar();

        let plain = write(d.path(), "proj-1.0.tar", &tar);

        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&tar).unwrap();
        let gz = write(d.path(), "proj-1.0.tar.gz", &gz.finish().unwrap());
        let tgz = write(d.path(), "proj-1.0.tgz", &fs::read(&gz).unwrap());

        let zst = ruzstd::encoding::compress_to_vec(&tar[..], ruzstd::encoding::CompressionLevel::Fastest);
        let zst = write(d.path(), "proj-1.0.tar.zst", &zst);

        let mut xz = Vec::new();
        lzma_rs::xz_compress(&mut &tar[..], &mut xz).unwrap();
        let xz = write(d.path(), "proj-1.0.tar.xz", &xz);

        for (i, archive) in [plain, gz, tgz, zst, xz].iter().enumerate() {
            let dest = d.path().join(format!("out{i}"));
            let root = extract(archive, &dest).unwrap_or_else(|e| panic!("{}: {e}", archive.display()));
            assert_eq!(root.file_name().unwrap(), "proj-1.0", "{}", archive.display());
            check(&root);
            assert!(!dest.join(".decompressed.tar").exists(), "no scratch file left behind");
        }
    }

    #[test]
    fn unpacks_bzip2_tarballs() {
        let d = tempfile::tempdir().unwrap();
        for name in ["proj-1.0.tar.bz2", "proj-1.0.tbz2"] {
            let archive = write(d.path(), name, SAMPLE_TAR_BZ2);
            let dest = d.path().join(format!("out-{name}"));
            let root = extract(&archive, &dest).unwrap();
            assert_eq!(root.file_name().unwrap(), "proj-1.0");
            check(&root);
        }
    }

    #[test]
    fn a_truncated_bzip2_tarball_is_an_error_not_a_partial_success() {
        let d = tempfile::tempdir().unwrap();
        let cut = &SAMPLE_TAR_BZ2[..SAMPLE_TAR_BZ2.len() / 2];
        let archive = write(d.path(), "cut.tar.bz2", cut);
        assert!(extract(&archive, &d.path().join("out")).is_err());
        let junk = write(d.path(), "junk.tar.bz2", b"not bzip2 data at all");
        assert!(extract(&junk, &d.path().join("out2")).is_err());
    }

    #[test]
    fn several_top_level_entries_are_kept_together() {
        let d = tempfile::tempdir().unwrap();
        let mut b = tar::Builder::new(Vec::new());
        for path in ["a.txt", "b.txt"] {
            let mut h = tar::Header::new_gnu();
            h.set_size(1);
            h.set_cksum();
            b.append_data(&mut h, path, &b"x"[..]).unwrap();
        }
        let archive = write(d.path(), "flat.tar", &b.into_inner().unwrap());
        let dest = d.path().join("out");
        assert_eq!(extract(&archive, &dest).unwrap(), dest);
        assert!(dest.join("a.txt").is_file() && dest.join("b.txt").is_file());
    }

    #[test]
    fn rejects_entries_that_escape_the_destination() {
        let d = tempfile::tempdir().unwrap();
        let mut b = tar::Builder::new(Vec::new());
        // `set_path` refuses `..`, so write the name straight into the header.
        let mut h = tar::Header::new_gnu();
        h.set_size(4);
        h.set_mode(0o644);
        h.as_old_mut().name[..9].copy_from_slice(b"../escape");
        h.set_cksum();
        b.append(&h, &b"evil"[..]).unwrap();
        let archive = write(d.path(), "evil.tar", &b.into_inner().unwrap());

        let dest = d.path().join("nested/out");
        let err = extract(&archive, &dest).unwrap_err();
        assert!(matches!(err, BuildError::UnsafeSource(_)), "{err}");
        assert!(!d.path().join("nested/escape").exists(), "nothing was written outside");
    }

    #[test]
    fn unsupported_and_corrupt_archives_are_reported() {
        let d = tempfile::tempdir().unwrap();
        let bz = write(d.path(), "x.tar.lz", b"LZIP");
        assert!(matches!(extract(&bz, &d.path().join("o1")), Err(BuildError::UnsupportedArchive(_))));
        let zip = write(d.path(), "x.zip", b"PK");
        assert!(matches!(extract(&zip, &d.path().join("o2")), Err(BuildError::UnsupportedArchive(_))));
        let bad = write(d.path(), "junk.tar.gz", b"definitely not gzip");
        assert!(extract(&bad, &d.path().join("o3")).is_err());
        let bad = write(d.path(), "junk.tar.zst", b"definitely not zstd");
        assert!(extract(&bad, &d.path().join("o4")).is_err());
    }
}

#[cfg(test)]
mod duplicate_tests {
    use super::*;

    fn tar_with(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (name, body) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_size(body.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, name, body.as_bytes()).unwrap();
        }
        b.into_inner().unwrap()
    }

    #[test]
    fn a_regular_file_listed_twice_takes_the_later_entry() {
        let t = tempfile::tempdir().unwrap();
        let a = t.path().join("a.tar");
        fs::write(&a, tar_with(&[("p-1/f", "old"), ("p-1/f", "new")])).unwrap();
        let root = extract(&a, &t.path().join("out")).unwrap();
        assert_eq!(fs::read_to_string(root.join("f")).unwrap(), "new");
    }

    #[test]
    fn a_file_cannot_replace_a_symlink_made_earlier() {
        let t = tempfile::tempdir().unwrap();
        let victim = t.path().join("victim");
        fs::write(&victim, "keep").unwrap();
        let mut b = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Symlink);
        h.set_size(0);
        h.set_cksum();
        b.append_link(&mut h, "p-1/f", victim.to_str().unwrap()).unwrap();
        let mut h = tar::Header::new_gnu();
        h.set_size(3);
        h.set_mode(0o644);
        h.set_cksum();
        b.append_data(&mut h, "p-1/f", &b"bad"[..]).unwrap();
        let a = t.path().join("a.tar");
        fs::write(&a, b.into_inner().unwrap()).unwrap();
        let _ = extract(&a, &t.path().join("out"));
        assert_eq!(fs::read_to_string(&victim).unwrap(), "keep", "nothing outside the source directory is touched");
    }
}

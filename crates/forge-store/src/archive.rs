//! Package archives: a zstd-compressed tar whose first entry is the manifest.
//!
//! Unpacking never uses tar's own extraction. Every entry must be listed in
//! the manifest, must have the declared type and content hash, and is created
//! exclusively (never overwriting, never through a symlink), so a hostile
//! archive cannot write outside its destination or tamper with other entries.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};
use std::path::Path;

use forge_schema::Libc;
use ruzstd::decoding::StreamingDecoder;
use ruzstd::encoding::{CompressionLevel, compress};
use sha2::{Digest, Sha256};
use tar::{Archive, Builder, EntryType, Header};

use crate::id::{StoreId, hex, sha256_hex};
use crate::manifest::{FileEntry, Kind, MANIFEST_PATH, Manifest};
use crate::{StoreError, io_ctx};

const MAX_MANIFEST_BYTES: u64 = 8 * 1024 * 1024;

fn bad_archive(msg: impl Into<String>) -> StoreError {
    StoreError::UnsafeArchive(msg.into())
}

// ---- packing ---------------------------------------------------------------

pub struct PackOptions {
    pub name: String,
    pub version: String,
    pub libc: Libc,
    /// Full SHA-256 hex naming the build inputs. If `None`, a hash of the
    /// package's content is used (fine for hand-made packs; builders pass
    /// the real input hash).
    pub hash: Option<String>,
    /// Store ids of run-time dependencies.
    pub deps: Vec<String>,
}

pub struct Packed {
    pub id: StoreId,
    /// SHA-256 of the archive file itself (what a repository index records).
    pub archive_sha256: String,
    pub manifest: Manifest,
}

fn walk(root: &Path, rel: &str, out: &mut Vec<FileEntry>) -> Result<(), StoreError> {
    let dir = if rel.is_empty() { root.to_path_buf() } else { root.join(rel) };
    let mut names: Vec<String> = fs::read_dir(&dir)
        .map_err(io_ctx(format!("reading {}", dir.display())))?
        .map(|e| {
            let e = e.map_err(io_ctx(format!("reading {}", dir.display())))?;
            e.file_name().into_string().map_err(|n| bad_archive(format!("non-UTF-8 file name {n:?}")))
        })
        .collect::<Result<_, _>>()?;
    names.sort();
    for name in names {
        let path = if rel.is_empty() { name } else { format!("{rel}/{name}") };
        let full = root.join(&path);
        let meta = fs::symlink_metadata(&full).map_err(io_ctx(format!("reading {}", full.display())))?;
        let ty = meta.file_type();
        if ty.is_dir() {
            out.push(FileEntry { path: path.clone(), kind: Kind::Dir, mode: 0o755, sha256: None, target: None });
            walk(root, &path, out)?;
        } else if ty.is_symlink() {
            let target = fs::read_link(&full).map_err(io_ctx(format!("reading link {}", full.display())))?;
            let target = target
                .to_str()
                .ok_or_else(|| bad_archive(format!("non-UTF-8 link target in {path}")))?
                .to_string();
            out.push(FileEntry { path, kind: Kind::Symlink, mode: 0o777, sha256: None, target: Some(target) });
        } else if ty.is_file() {
            let mut f = File::open(&full).map_err(io_ctx(format!("opening {}", full.display())))?;
            let mut h = Sha256::new();
            io::copy(&mut f, &mut HashWriter(&mut h)).map_err(io_ctx(format!("reading {}", full.display())))?;
            let mode = if meta.permissions().mode() & 0o111 != 0 { 0o755 } else { 0o644 };
            out.push(FileEntry { path, kind: Kind::File, mode, sha256: Some(hex(&h.finalize())), target: None });
        } else {
            return Err(bad_archive(format!("`{path}` is not a regular file, directory or symlink")));
        }
    }
    Ok(())
}

struct HashWriter<'a>(&'a mut Sha256);
impl Write for HashWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.update(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn content_hash(opts: &PackOptions, files: &[FileEntry]) -> String {
    let mut text = format!("{}\n{}\n{:?}\n{}\n", opts.name, opts.version, opts.libc, opts.deps.join(","));
    for f in files {
        text.push_str(&format!(
            "{} {:?} {:o} {} {}\n",
            f.path,
            f.kind,
            f.mode,
            f.sha256.as_deref().unwrap_or("-"),
            f.target.as_deref().unwrap_or("-")
        ));
    }
    sha256_hex(text.as_bytes())
}

fn header(kind: EntryType, mode: u32, size: u64) -> Header {
    let mut h = Header::new_gnu();
    h.set_entry_type(kind);
    h.set_mode(mode);
    h.set_size(size);
    h.set_mtime(0);
    h.set_uid(0);
    h.set_gid(0);
    h
}

/// Pack the directory `src` into a package archive at `out`. The output is
/// deterministic: packing the same tree twice gives byte-identical archives.
pub fn pack_dir(src: &Path, opts: &PackOptions, out: &Path) -> Result<Packed, StoreError> {
    let mut files = Vec::new();
    walk(src, "", &mut files)?;
    files.sort_by(|a, b| a.path.cmp(&b.path));

    let hash = opts.hash.clone().unwrap_or_else(|| content_hash(opts, &files));
    let manifest = Manifest {
        name: opts.name.clone(),
        version: opts.version.clone(),
        libc: opts.libc,
        hash,
        deps: opts.deps.clone(),
        files,
    };
    manifest.validate()?;
    let id = manifest.id()?;
    let text = toml::to_string(&manifest).expect("manifest serializes");

    let mut tmp = out.as_os_str().to_owned();
    tmp.push(".tar-tmp");
    let tmp = std::path::PathBuf::from(tmp);
    let ctx = |what: &str| format!("{what} {}", out.display());

    let result = (|| -> Result<(), StoreError> {
        let mut b = Builder::new(File::create(&tmp).map_err(io_ctx(ctx("creating tar for")))?);
        let mut h = header(EntryType::Regular, 0o644, text.len() as u64);
        b.append_data(&mut h, MANIFEST_PATH, text.as_bytes()).map_err(io_ctx(ctx("writing")))?;
        for f in &manifest.files {
            match f.kind {
                Kind::Dir => {
                    let mut h = header(EntryType::Directory, 0o755, 0);
                    b.append_data(&mut h, &f.path, io::empty()).map_err(io_ctx(ctx("writing")))?;
                }
                Kind::Symlink => {
                    let mut h = header(EntryType::Symlink, 0o777, 0);
                    h.set_link_name(f.target.as_deref().expect("validated")).map_err(io_ctx(ctx("writing")))?;
                    b.append_data(&mut h, &f.path, io::empty()).map_err(io_ctx(ctx("writing")))?;
                }
                Kind::File => {
                    let full = src.join(&f.path);
                    let file = File::open(&full).map_err(io_ctx(format!("opening {}", full.display())))?;
                    let size = file.metadata().map_err(io_ctx(format!("reading {}", full.display())))?.len();
                    let mut h = header(EntryType::Regular, f.mode, size);
                    b.append_data(&mut h, &f.path, file.take(size)).map_err(io_ctx(ctx("writing")))?;
                }
            }
        }
        b.finish().map_err(io_ctx(ctx("finishing")))?;
        let tar = File::open(&tmp).map_err(io_ctx(ctx("reopening tar for")))?;
        let dst = File::create(out).map_err(io_ctx(ctx("creating")))?;
        compress(tar, dst, CompressionLevel::Fastest);
        Ok(())
    })();
    let _ = fs::remove_file(&tmp);
    result?;

    let mut f = File::open(out).map_err(io_ctx(ctx("reading")))?;
    let mut h = Sha256::new();
    io::copy(&mut f, &mut HashWriter(&mut h)).map_err(io_ctx(ctx("hashing")))?;
    Ok(Packed { id, archive_sha256: hex(&h.finalize()), manifest })
}

// ---- reading ---------------------------------------------------------------

fn read_manifest_entry<R: Read>(entry: &mut tar::Entry<'_, R>) -> Result<(Manifest, String), StoreError> {
    let path = entry.path().map_err(io_ctx("reading the first archive entry"))?;
    if path.to_str() != Some(MANIFEST_PATH) {
        return Err(bad_archive(format!("first entry must be {MANIFEST_PATH}, found {}", path.display())));
    }
    if entry.header().entry_type() != EntryType::Regular || entry.size() > MAX_MANIFEST_BYTES {
        return Err(bad_archive("manifest is not a regular file of reasonable size"));
    }
    let mut text = String::new();
    entry.read_to_string(&mut text).map_err(io_ctx("reading the manifest"))?;
    let manifest: Manifest = toml::from_str(&text).map_err(|e| bad_archive(format!("bad manifest: {e}")))?;
    manifest.validate()?;
    Ok((manifest, text))
}

fn decoder<R: Read>(reader: R) -> Result<StreamingDecoder<R, ruzstd::decoding::FrameDecoder>, StoreError> {
    StreamingDecoder::new(reader).map_err(|e| bad_archive(format!("not a valid zstd archive: {e}")))
}

/// Read just the manifest of an archive, without unpacking it.
pub fn read_manifest<R: Read>(reader: R) -> Result<Manifest, StoreError> {
    let mut ar = Archive::new(decoder(reader)?);
    let mut entries = ar.entries().map_err(io_ctx("reading the archive"))?;
    let mut first = entries
        .next()
        .ok_or_else(|| bad_archive("archive is empty"))?
        .map_err(io_ctx("reading the archive"))?;
    Ok(read_manifest_entry(&mut first)?.0)
}

fn mode_for(f: &FileEntry) -> u32 {
    match f.kind {
        Kind::Dir => 0o555,
        _ if f.mode & 0o111 != 0 => 0o555,
        _ => 0o444,
    }
}

/// Unpack and fully verify an archive into the new directory `dest`
/// (which must not exist). On error `dest` may be partly filled; the caller
/// removes it. Everything is made read-only on success.
pub(crate) fn unpack<R: Read>(reader: R, dest: &Path) -> Result<Manifest, StoreError> {
    let mut ar = Archive::new(decoder(reader)?);
    let mut entries = ar.entries().map_err(io_ctx("reading the archive"))?;
    let mut first = entries
        .next()
        .ok_or_else(|| bad_archive("archive is empty"))?
        .map_err(io_ctx("reading the archive"))?;
    let (manifest, text) = read_manifest_entry(&mut first)?;
    drop(first);

    let ctx = |what: &str, p: &Path| format!("{what} {}", p.display());
    fs::create_dir(dest).map_err(io_ctx(ctx("creating", dest)))?;
    let meta_dir = dest.join(".forge");
    fs::create_dir(&meta_dir).map_err(io_ctx(ctx("creating", &meta_dir)))?;
    let manifest_file = dest.join(MANIFEST_PATH);
    fs::write(&manifest_file, &text).map_err(io_ctx(ctx("writing", &manifest_file)))?;
    fs::set_permissions(&manifest_file, fs::Permissions::from_mode(0o444))
        .map_err(io_ctx(ctx("protecting", &manifest_file)))?;

    let by_path: BTreeMap<&str, &FileEntry> = manifest.files.iter().map(|f| (f.path.as_str(), f)).collect();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut dirs = vec![dest.to_path_buf(), meta_dir];

    for entry in entries {
        let mut entry = entry.map_err(io_ctx("reading the archive"))?;
        let raw = entry
            .path()
            .map_err(io_ctx("reading an entry name"))?
            .to_str()
            .ok_or_else(|| bad_archive("non-UTF-8 entry name"))?
            .to_string();
        let path = raw.strip_suffix('/').unwrap_or(&raw).to_string();
        let fe = by_path
            .get(path.as_str())
            .ok_or_else(|| bad_archive(format!("`{path}` is not listed in the manifest")))?;
        if !seen.insert(path.clone()) {
            return Err(bad_archive(format!("`{path}` appears twice")));
        }

        let target = dest.join(&path);
        let parent = target.parent().expect("has parent");
        let pm = fs::symlink_metadata(parent).map_err(io_ctx(ctx("checking", parent)))?;
        if !pm.is_dir() {
            return Err(bad_archive(format!("parent of `{path}` is not a real directory")));
        }

        let ty = entry.header().entry_type();
        match (fe.kind, ty) {
            (Kind::Dir, EntryType::Directory) => {
                fs::create_dir(&target).map_err(io_ctx(ctx("creating", &target)))?;
                dirs.push(target);
            }
            (Kind::Symlink, EntryType::Symlink) => {
                let link = entry
                    .link_name()
                    .map_err(io_ctx("reading a link target"))?
                    .ok_or_else(|| bad_archive(format!("`{path}` has no link target")))?;
                if link.to_str() != fe.target.as_deref() {
                    return Err(bad_archive(format!("link target of `{path}` differs from the manifest")));
                }
                symlink(fe.target.as_deref().expect("validated"), &target)
                    .map_err(io_ctx(ctx("linking", &target)))?;
            }
            (Kind::File, EntryType::Regular) => {
                let mut out = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&target)
                    .map_err(io_ctx(ctx("creating", &target)))?;
                let mut hasher = Sha256::new();
                let mut buf = [0u8; 64 * 1024];
                loop {
                    let n = entry.read(&mut buf).map_err(io_ctx(format!("reading `{path}`")))?;
                    if n == 0 {
                        break;
                    }
                    hasher.update(&buf[..n]);
                    out.write_all(&buf[..n]).map_err(io_ctx(ctx("writing", &target)))?;
                }
                drop(out);
                if Some(hex(&hasher.finalize()).as_str()) != fe.sha256.as_deref() {
                    return Err(StoreError::FileHashMismatch { path });
                }
                fs::set_permissions(&target, fs::Permissions::from_mode(mode_for(fe)))
                    .map_err(io_ctx(ctx("protecting", &target)))?;
            }
            _ => return Err(bad_archive(format!("`{path}` has the wrong entry type for its manifest kind"))),
        }
    }

    if seen.len() != manifest.files.len() {
        return Err(bad_archive(format!(
            "archive is missing {} entries listed in the manifest",
            manifest.files.len() - seen.len()
        )));
    }
    for d in dirs.iter().rev() {
        fs::set_permissions(d, fs::Permissions::from_mode(0o555)).map_err(io_ctx(ctx("protecting", d)))?;
    }
    Ok(manifest)
}

#[cfg(test)]
mod tests;

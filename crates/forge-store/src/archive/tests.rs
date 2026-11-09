use std::io::Cursor;
use std::os::unix::net::UnixListener;

use ruzstd::encoding::compress_to_vec;
use tempfile::TempDir;

use super::*;

fn sample_tree(root: &Path) {
    fs::create_dir_all(root.join("usr/bin")).unwrap();
    fs::create_dir_all(root.join("usr/share")).unwrap();
    fs::create_dir_all(root.join("var/empty")).unwrap();
    fs::write(root.join("usr/bin/hello"), "#!/bin/sh\necho hi\n").unwrap();
    fs::set_permissions(root.join("usr/bin/hello"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(root.join("usr/share/doc.txt"), "docs\n").unwrap();
    symlink("hello", root.join("usr/bin/hi")).unwrap();
}

fn opts() -> PackOptions {
    PackOptions { name: "hello".into(), version: "1.0".into(), libc: Libc::Musl, hash: None, deps: vec![] }
}

fn pack(src: &Path, out: &Path) -> (Vec<u8>, Packed) {
    let packed = pack_dir(src, &opts(), out).unwrap();
    (fs::read(out).unwrap(), packed)
}

fn mode(p: &Path) -> u32 {
    fs::symlink_metadata(p).unwrap().permissions().mode() & 0o7777
}

// ---- crafting hostile archives --------------------------------------------

const H: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn entry(path: &str, kind: Kind, content: &[u8]) -> FileEntry {
    FileEntry {
        path: path.into(),
        kind,
        mode: if kind == Kind::Dir { 0o755 } else { 0o644 },
        sha256: (kind == Kind::File).then(|| sha256_hex(content)),
        target: (kind == Kind::Symlink).then(|| "x".to_string()),
    }
}

fn manifest(files: Vec<FileEntry>) -> Manifest {
    Manifest { name: "evil".into(), version: "1".into(), libc: Libc::Musl, hash: H.into(), deps: vec![], files }
}

/// Build an archive from a manifest and an arbitrary list of tar entries.
fn craft(m: &Manifest, entries: &[(&str, EntryType, &[u8])]) -> Vec<u8> {
    let text = toml::to_string(m).unwrap();
    let mut b = Builder::new(Vec::new());
    let mut h = header(EntryType::Regular, 0o644, text.len() as u64);
    b.append_data(&mut h, MANIFEST_PATH, text.as_bytes()).unwrap();
    for (path, ty, content) in entries {
        let mut h = header(*ty, 0o644, content.len() as u64);
        if *ty == EntryType::Symlink {
            h.set_link_name("x").unwrap();
        }
        b.append_data(&mut h, path, *content).unwrap();
    }
    compress_to_vec(Cursor::new(b.into_inner().unwrap()), CompressionLevel::Fastest)
}

fn unpack_bytes(bytes: &[u8]) -> (Result<Manifest, StoreError>, TempDir) {
    let d = tempfile::tempdir().unwrap();
    let r = unpack(bytes, &d.path().join("out"));
    (r, d)
}

fn expect_unsafe(bytes: &[u8], needle: &str) {
    let (r, _d) = unpack_bytes(bytes);
    let msg = r.expect_err("must be rejected").to_string();
    assert!(msg.contains(needle), "expected `{needle}` in: {msg}");
}

// ---- tests -----------------------------------------------------------------

#[test]
fn pack_then_unpack_round_trips() {
    let d = tempfile::tempdir().unwrap();
    let src = d.path().join("src");
    fs::create_dir(&src).unwrap();
    sample_tree(&src);
    let (bytes, packed) = pack(&src, &d.path().join("hello.tar.zst"));
    assert_eq!(packed.id.name(), "hello");

    let dest = d.path().join("entry");
    let m = unpack(&bytes[..], &dest).unwrap();
    assert_eq!(m, packed.manifest);
    assert_eq!(fs::read_to_string(dest.join("usr/bin/hello")).unwrap(), "#!/bin/sh\necho hi\n");
    assert_eq!(fs::read_link(dest.join("usr/bin/hi")).unwrap(), Path::new("hello"));
    assert!(dest.join("var/empty").is_dir());
    assert_eq!(mode(&dest.join("usr/bin/hello")), 0o555, "executables are r-x");
    assert_eq!(mode(&dest.join("usr/share/doc.txt")), 0o444, "other files are r--");
    assert_eq!(mode(&dest.join("usr")), 0o555, "directories are read-only");
    assert_eq!(mode(&dest), 0o555);
    assert!(dest.join(MANIFEST_PATH).is_file());
    assert_eq!(read_manifest(&bytes[..]).unwrap(), packed.manifest);
    // Make it deletable again so the temp dir can clean up.
    crate::store::make_writable(&dest).unwrap();
}

#[test]
fn packing_is_deterministic() {
    let d = tempfile::tempdir().unwrap();
    let src = d.path().join("src");
    fs::create_dir(&src).unwrap();
    sample_tree(&src);
    let (a, pa) = pack(&src, &d.path().join("a.tar.zst"));
    let (b, pb) = pack(&src, &d.path().join("b.tar.zst"));
    assert_eq!(a, b);
    assert_eq!(pa.archive_sha256, pb.archive_sha256);
    assert_eq!(pa.id, pb.id);
    assert_eq!(pa.archive_sha256, sha256_hex(&a));
}

#[test]
fn different_content_gives_a_different_id() {
    let d = tempfile::tempdir().unwrap();
    let src = d.path().join("src");
    fs::create_dir(&src).unwrap();
    sample_tree(&src);
    let (_, one) = pack(&src, &d.path().join("1.tar.zst"));
    fs::write(src.join("usr/share/doc.txt"), "changed\n").unwrap();
    let (_, two) = pack(&src, &d.path().join("2.tar.zst"));
    assert_ne!(one.id, two.id);
}

#[test]
fn pack_rejects_special_files() {
    let d = tempfile::tempdir().unwrap();
    let src = d.path().join("src");
    fs::create_dir(&src).unwrap();
    let _sock = UnixListener::bind(src.join("sock")).unwrap();
    assert!(pack_dir(&src, &opts(), &d.path().join("x.tar.zst")).is_err());
}

#[test]
fn pack_rejects_the_reserved_directory() {
    let d = tempfile::tempdir().unwrap();
    let src = d.path().join("src");
    fs::create_dir_all(src.join(".forge")).unwrap();
    assert!(pack_dir(&src, &opts(), &d.path().join("x.tar.zst")).is_err());
}

#[test]
fn rejects_entries_missing_from_the_manifest() {
    let m = manifest(vec![entry("a", Kind::File, b"a")]);
    let bytes = craft(&m, &[("a", EntryType::Regular, b"a"), ("sneaky", EntryType::Regular, b"x")]);
    expect_unsafe(&bytes, "not listed in the manifest");
}

#[test]
fn rejects_content_that_does_not_match_its_hash() {
    let m = manifest(vec![entry("a", Kind::File, b"expected")]);
    let bytes = craft(&m, &[("a", EntryType::Regular, b"tampered")]);
    let (r, _d) = unpack_bytes(&bytes);
    assert!(matches!(r, Err(StoreError::FileHashMismatch { path }) if path == "a"));
}

#[test]
fn rejects_archives_missing_listed_entries() {
    let m = manifest(vec![entry("a", Kind::File, b"a"), entry("b", Kind::File, b"b")]);
    expect_unsafe(&craft(&m, &[("a", EntryType::Regular, b"a")]), "missing 1 entries");
}

#[test]
fn rejects_duplicate_entries() {
    let m = manifest(vec![entry("a", Kind::File, b"a")]);
    let bytes = craft(&m, &[("a", EntryType::Regular, b"a"), ("a", EntryType::Regular, b"a")]);
    expect_unsafe(&bytes, "appears twice");
}

#[test]
fn rejects_an_entry_type_that_differs_from_the_manifest() {
    // The manifest promises a directory; the archive swaps in a symlink.
    let m = manifest(vec![entry("d", Kind::Dir, b"")]);
    expect_unsafe(&craft(&m, &[("d", EntryType::Symlink, b"")]), "wrong entry type");
    let m = manifest(vec![entry("f", Kind::File, b"x")]);
    expect_unsafe(&craft(&m, &[("f", EntryType::Symlink, b"")]), "wrong entry type");
}

#[test]
fn rejects_writing_through_a_symlink() {
    // `l` is a symlink and `l/evil` would be written through it. The manifest
    // validator refuses it (no listed parent directory) before anything is unpacked.
    let m = manifest(vec![entry("l", Kind::Symlink, b""), entry("l/evil", Kind::File, b"x")]);
    expect_unsafe(&craft(&m, &[("l", EntryType::Symlink, b""), ("l/evil", EntryType::Regular, b"x")]), "parent");
}

#[test]
fn rejects_a_directory_entry_swapped_for_a_symlink_before_its_children() {
    let m = manifest(vec![entry("d", Kind::Dir, b""), entry("d/f", Kind::File, b"x")]);
    let bytes = craft(&m, &[("d", EntryType::Symlink, b""), ("d/f", EntryType::Regular, b"x")]);
    expect_unsafe(&bytes, "wrong entry type");
}

#[test]
fn rejects_garbage_and_a_missing_manifest() {
    expect_unsafe(b"this is not zstd at all", "zstd");
    let mut b = Builder::new(Vec::new());
    let mut h = header(EntryType::Regular, 0o644, 1);
    b.append_data(&mut h, "usr/bin/x", &b"x"[..]).unwrap();
    let bytes = compress_to_vec(Cursor::new(b.into_inner().unwrap()), CompressionLevel::Fastest);
    expect_unsafe(&bytes, "first entry must be");
}

#[test]
fn refuses_to_unpack_into_an_existing_directory() {
    let d = tempfile::tempdir().unwrap();
    let m = manifest(vec![]);
    let bytes = craft(&m, &[]);
    fs::create_dir(d.path().join("out")).unwrap();
    assert!(unpack(&bytes[..], &d.path().join("out")).is_err());
}

#[test]
fn setuid_bits_in_the_manifest_never_reach_the_disk() {
    let mut f = entry("su", Kind::File, b"x");
    f.mode = 0o4755;
    let m = manifest(vec![f]);
    let bytes = craft(&m, &[("su", EntryType::Regular, b"x")]);
    let (r, d) = unpack_bytes(&bytes);
    r.unwrap();
    let p = d.path().join("out/su");
    assert_eq!(mode(&p), 0o555, "executable but not setuid");
    crate::store::make_writable(&d.path().join("out")).unwrap();
}

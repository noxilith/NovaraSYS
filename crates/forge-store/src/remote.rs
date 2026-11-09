//! Repositories served over HTTP(S): a base URL holding `index.toml`, `index.toml.sig` and the archives
//! as flat files (a GitHub Release is exactly that).
//!
//! The transport needs no authentication of its own. Everything that matters is checked after the
//! download: the index against the trusted keys, each archive against the hash in that index. A man in
//! the middle can refuse to deliver, or replay an old index (which the serial check catches), but cannot
//! make anything else install. That is why BusyBox's `wget` (which does not verify certificates) is an
//! acceptable fallback where there is no `curl`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use crate::repo::{IndexEntry, Loaded, Trust, verify_index};
use crate::{StoreError, io_ctx};

pub fn is_url(spec: &str) -> bool {
    spec.starts_with("https://") || spec.starts_with("http://")
}

/// Downloads. Tests substitute an in-memory one.
pub trait Fetch: Send + Sync {
    /// A small file, in memory (the index and its signature).
    fn get(&self, url: &str) -> Result<Vec<u8>, StoreError>;
    /// A large file, to `dest` (an archive).
    fn download(&self, url: &str, dest: &Path) -> Result<(), StoreError>;
}

/// The real thing: `curl` if there is one, otherwise BusyBox `wget`.
pub struct SystemFetch;

fn in_path(tool: &str) -> bool {
    std::env::var_os("PATH").map(|p| std::env::split_paths(&p).any(|d| d.join(tool).is_file())).unwrap_or(false)
}

fn fetch_err(url: &str, reason: impl Into<String>) -> StoreError {
    StoreError::Fetch { url: url.to_string(), reason: reason.into() }
}

impl SystemFetch {
    fn command(url: &str, out: &str) -> Result<Command, StoreError> {
        if in_path("curl") {
            let mut c = Command::new("curl");
            c.args(["-fSL", "--retry", "2", "--connect-timeout", "20", "-o", out, url]);
            Ok(c)
        } else if in_path("wget") {
            let mut c = Command::new("wget");
            c.args(["-O", out, url]);
            Ok(c)
        } else {
            Err(fetch_err(url, "neither curl nor wget is installed"))
        }
    }
}

impl Fetch for SystemFetch {
    fn get(&self, url: &str) -> Result<Vec<u8>, StoreError> {
        let mut c = Self::command(url, "-")?;
        // Both tools write the body to stdout for "-"; their own progress goes to stderr, which we hide
        // for these small files.
        let out = c.stdin(Stdio::null()).stderr(Stdio::piped()).output().map_err(|e| fetch_err(url, e.to_string()))?;
        if out.status.success() {
            Ok(out.stdout)
        } else {
            Err(fetch_err(url, String::from_utf8_lossy(&out.stderr).trim().to_string()))
        }
    }

    fn download(&self, url: &str, dest: &Path) -> Result<(), StoreError> {
        let mut c = Self::command(url, &dest.display().to_string())?;
        // Archives can be hundreds of megabytes: let the tool show its progress.
        let status = c.stdin(Stdio::null()).status().map_err(|e| fetch_err(url, e.to_string()))?;
        if status.success() { Ok(()) } else { Err(fetch_err(url, format!("the download tool exited with {status}"))) }
    }
}

/// A shareable handle on a downloader (so `Options` can stay `Clone` and `Debug`).
#[derive(Clone)]
pub struct FetchHandle(pub Arc<dyn Fetch>);

impl Default for FetchHandle {
    fn default() -> Self {
        FetchHandle(Arc::new(SystemFetch))
    }
}

impl std::fmt::Debug for FetchHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FetchHandle")
    }
}

#[derive(Debug, Clone)]
pub struct RemoteRepo {
    base: String,
}

impl RemoteRepo {
    pub fn new(base: impl Into<String>) -> Self {
        RemoteRepo { base: base.into().trim_end_matches('/').to_string() }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// How errors name this repository.
    pub fn label(&self) -> PathBuf {
        PathBuf::from(&self.base)
    }

    /// Download the index (and, unless `AllowUnsigned`, its signature) and verify it.
    pub fn load(&self, fetch: &dyn Fetch, trust: &Trust) -> Result<Loaded, StoreError> {
        let index_url = format!("{}/index.toml", self.base);
        let bytes = fetch.get(&index_url)?;
        let sig = match trust {
            Trust::AllowUnsigned => None,
            // A missing signature file is the "unsigned" error, not a download failure.
            Trust::Require(_) => fetch.get(&format!("{}/index.toml.sig", self.base)).ok().and_then(|b| String::from_utf8(b).ok()),
        };
        verify_index(&self.label(), Path::new(&index_url), &bytes, sig.as_deref(), trust)
    }

    /// Where an archive of this repository is downloaded from. The name comes from the (verified) index,
    /// but it is still only ever a bare file name.
    pub fn archive_url(&self, e: &IndexEntry) -> Result<String, StoreError> {
        if e.archive.is_empty() || e.archive.contains(['/', '\\']) || e.archive.starts_with('.') {
            return Err(StoreError::Invalid { what: "archive name in an index", value: e.archive.clone() });
        }
        Ok(format!("{}/{}", self.base, e.archive))
    }
}

/// Download `url` into a fresh temporary file in `dir`, returning its path (the caller removes it).
pub fn download_to_temp(fetch: &dyn Fetch, url: &str, dir: &Path) -> Result<PathBuf, StoreError> {
    fs::create_dir_all(dir).map_err(io_ctx(format!("creating {}", dir.display())))?;
    let tmp = dir.join(format!(".download-{}", std::process::id()));
    let _ = fs::remove_file(&tmp);
    if let Err(e) = fetch.download(url, &tmp) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(tmp)
}

/// A repository named by a directory or by an HTTP(S) base URL.
#[derive(Debug, Clone)]
pub enum Repository {
    Local(crate::repo::LocalRepo),
    Remote(RemoteRepo),
}

impl Repository {
    /// `https://...` and `http://...` are remote; anything else is a directory.
    pub fn parse(spec: &str) -> Self {
        if is_url(spec) { Repository::Remote(RemoteRepo::new(spec)) } else { Repository::Local(crate::repo::LocalRepo::new(spec)) }
    }

    pub fn load(&self, fetch: &dyn Fetch, trust: &Trust) -> Result<Loaded, StoreError> {
        match self {
            Repository::Local(r) => r.load(trust),
            Repository::Remote(r) => r.load(fetch, trust),
        }
    }

    /// Open an archive. A remote one is downloaded into `scratch` first and unlinked once opened, so
    /// nothing is left behind; the caller still verifies it against the hash in the index.
    pub fn open_archive(&self, e: &IndexEntry, fetch: &dyn Fetch, scratch: &Path) -> Result<fs::File, StoreError> {
        match self {
            Repository::Local(r) => r.open_archive(e),
            Repository::Remote(r) => {
                let url = r.archive_url(e)?;
                let tmp = download_to_temp(fetch, &url, scratch)?;
                let file = fs::File::open(&tmp).map_err(io_ctx(format!("opening {}", tmp.display())));
                let _ = fs::remove_file(&tmp);
                file
            }
        }
    }
}

impl PartialEq for FetchHandle {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for FetchHandle {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::{IndexEntry, LocalRepo};
    use crate::sign::{Keyring, SecretKey};
    use crate::testutil::make_pkg;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    /// Serves a local repository directory as if it were at `https://example.test/repo`.
    struct Served {
        dir: PathBuf,
        log: Mutex<Vec<String>>,
        missing: Vec<&'static str>,
    }

    impl Served {
        fn path(&self, url: &str) -> Result<PathBuf, StoreError> {
            self.log.lock().unwrap().push(url.to_string());
            let name = url.strip_prefix("https://example.test/repo/").ok_or_else(|| fetch_err(url, "unknown host"))?;
            if self.missing.contains(&name) {
                return Err(fetch_err(url, "404"));
            }
            Ok(self.dir.join(name))
        }
    }

    impl Fetch for Served {
        fn get(&self, url: &str) -> Result<Vec<u8>, StoreError> {
            fs::read(self.path(url)?).map_err(|e| fetch_err(url, e.to_string()))
        }
        fn download(&self, url: &str, dest: &Path) -> Result<(), StoreError> {
            fs::copy(self.path(url)?, dest).map(drop).map_err(|e| fetch_err(url, e.to_string()))
        }
    }

    fn repo_with_one_package(dir: &Path, key: &SecretKey) -> IndexEntry {
        make_pkg(dir, "hello", "1.0", &[("usr/bin/hello", "hi")], &[]);
        LocalRepo::new(dir.join("repo")).publish(&dir.join("hello-1.0.tar.zst"), Some(key)).unwrap()
    }

    fn ring(key: &SecretKey) -> Trust {
        let mut r = Keyring::empty();
        r.insert(key.public());
        Trust::Require(r)
    }

    #[test]
    fn a_remote_signed_index_loads_and_names_its_archives_by_url() {
        let t = tempfile::tempdir().unwrap();
        let key = SecretKey::from_seed([3; 32]);
        let entry = repo_with_one_package(t.path(), &key);
        let served = Served { dir: t.path().join("repo"), log: Default::default(), missing: vec![] };
        let repo = RemoteRepo::new("https://example.test/repo/");
        let loaded = repo.load(&served, &ring(&key)).unwrap();
        assert_eq!(loaded.index.packages.len(), 1);
        assert_eq!(loaded.signed_by.as_deref(), Some(key.public().id().as_str()));
        assert_eq!(repo.archive_url(&entry).unwrap(), format!("https://example.test/repo/{}", entry.archive));
    }

    #[test]
    fn untrusted_unsigned_and_tampered_remote_indexes_are_refused() {
        let t = tempfile::tempdir().unwrap();
        let key = SecretKey::from_seed([3; 32]);
        repo_with_one_package(t.path(), &key);
        let repo = RemoteRepo::new("https://example.test/repo");
        let served = |missing: Vec<&'static str>| Served { dir: t.path().join("repo"), log: Default::default(), missing };

        let other = SecretKey::from_seed([4; 32]);
        assert!(matches!(repo.load(&served(vec![]), &ring(&other)), Err(StoreError::UnknownKey { .. })));
        assert!(matches!(repo.load(&served(vec!["index.toml.sig"]), &ring(&key)), Err(StoreError::Unsigned(_))));
        assert!(repo.load(&served(vec!["index.toml.sig"]), &Trust::AllowUnsigned).is_ok());

        let idx = t.path().join("repo/index.toml");
        let text = fs::read_to_string(&idx).unwrap();
        fs::write(&idx, format!("{text}\n# tampered\n")).unwrap();
        assert!(matches!(repo.load(&served(vec![]), &ring(&key)), Err(StoreError::BadSignature { .. })));
    }

    #[test]
    fn a_download_failure_names_the_url() {
        let t = tempfile::tempdir().unwrap();
        let served = Served { dir: t.path().to_path_buf(), log: Default::default(), missing: vec!["index.toml"] };
        let err = RemoteRepo::new("https://example.test/repo").load(&served, &Trust::AllowUnsigned).unwrap_err();
        assert!(err.to_string().contains("https://example.test/repo/index.toml"), "{err}");
    }

    #[test]
    fn archive_names_from_an_index_cannot_leave_the_repository() {
        let repo = RemoteRepo::new("https://example.test/repo");
        let mut e = IndexEntry {
            name: "x".into(),
            version: "1".into(),
            libc: forge_schema::Libc::Musl,
            hash: "0".repeat(64),
            archive: "../../evil".into(),
            archive_sha256: "0".repeat(64),
            deps: vec![],
        };
        for bad in ["../../evil", "a/b", "", ".hidden", "a\\b"] {
            e.archive = bad.into();
            assert!(repo.archive_url(&e).is_err(), "{bad:?}");
        }
        e.archive = "fine-1.0-abc.tar.zst".into();
        assert!(repo.archive_url(&e).is_ok());
    }

    #[test]
    fn urls_are_told_from_paths() {
        assert!(is_url("https://github.com/x/y/releases/download/repo"));
        assert!(is_url("http://localhost:8000/r"));
        assert!(!is_url("/srv/repo"));
        assert!(!is_url("file:///srv/repo"));
        assert!(!is_url("repo/tools"));
    }

    #[test]
    fn a_downloaded_archive_lands_in_a_temp_file_and_failures_leave_nothing() {
        let t = tempfile::tempdir().unwrap();
        let key = SecretKey::from_seed([3; 32]);
        let entry = repo_with_one_package(t.path(), &key);
        let served = Served { dir: t.path().join("repo"), log: Default::default(), missing: vec![] };
        let url = format!("https://example.test/repo/{}", entry.archive);
        let tmp = download_to_temp(&served, &url, &t.path().join("dl")).unwrap();
        assert!(tmp.is_file());
        let missing = Served { dir: t.path().join("repo"), log: Default::default(), missing: vec!["nope.tar.zst"] };
        assert!(download_to_temp(&missing, "https://example.test/repo/nope.tar.zst", &t.path().join("dl2")).is_err());
        assert!(!t.path().join("dl2").join(format!(".download-{}", std::process::id())).exists());
        let _ = BTreeMap::<u8, u8>::new();
    }
}

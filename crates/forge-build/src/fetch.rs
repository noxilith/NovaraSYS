//! Source fetching: a content-addressed cache, so a source is only ever used
//! if its SHA-256 matches what the recipe pins.
//!
//! Local paths and `file://` URLs are copied; `http(s)://` URLs are
//! downloaded with `curl` (an interim choice: no TLS stack is linked into
//! `forge` yet). The download is untrusted until its checksum matches, so
//! the transport only needs to be reasonable, not trustworthy.

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::Command;

use forge_schema::Source;
use sha2::{Digest, Sha256};

use crate::{BuildError, io_ctx};

pub struct SourceCache {
    dir: PathBuf,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hash_file(path: &Path) -> io::Result<String> {
    let mut f = File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            return Ok(hex(&h.finalize()));
        }
        h.update(&buf[..n]);
    }
}

/// The last path segment of a URL, restricted to safe characters.
pub(crate) fn file_name_of(url: &str) -> Result<String, BuildError> {
    let no_query = url.split(['?', '#']).next().unwrap_or("");
    let name = no_query.rsplit('/').next().unwrap_or("");
    let ok = !name.is_empty()
        && !name.starts_with('.')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-' | '~'));
    if ok {
        Ok(name.to_string())
    } else {
        Err(BuildError::Recipe(format!("cannot derive a safe file name from source url `{url}`")))
    }
}

/// Download `url` to `dest` with curl. Redirects are followed, but never
/// from https to a different protocol.
fn download(url: &str, dest: &Path) -> Result<(), BuildError> {
    let out = Command::new("curl")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--max-redirs",
            "5",
            "--proto",
            "=http,https",
            "--proto-redir",
            "=https",
            "--connect-timeout",
            "20",
            "--max-time",
            "900",
            "--output",
        ])
        .arg(dest)
        .arg(url)
        .output()
        .map_err(|e| BuildError::Download {
            url: url.to_string(),
            message: if e.kind() == io::ErrorKind::NotFound {
                "curl is required to download sources but was not found".to_string()
            } else {
                e.to_string()
            },
        })?;
    if out.status.success() {
        Ok(())
    } else {
        Err(BuildError::Download {
            url: url.to_string(),
            message: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        })
    }
}

impl SourceCache {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        SourceCache { dir: dir.into() }
    }

    /// Where a source lives in the cache: `<sha256>-<file name>`.
    pub fn path_for(&self, source: &Source) -> Result<PathBuf, BuildError> {
        Ok(self.dir.join(format!("{}-{}", source.sha256.to_ascii_lowercase(), file_name_of(&source.url)?)))
    }

    /// Return the cached, verified file for `source`, fetching it first if
    /// needed. A file that does not match the pinned checksum is never kept.
    pub fn fetch(&self, source: &Source) -> Result<PathBuf, BuildError> {
        let cached = self.path_for(source)?;
        let expected = source.sha256.to_ascii_lowercase();

        if cached.is_file() {
            if hash_file(&cached).map_err(io_ctx(format!("reading {}", cached.display())))? == expected {
                return Ok(cached);
            }
            // A damaged cache entry is never trusted; fetch it again.
            fs::remove_file(&cached).map_err(io_ctx(format!("removing {}", cached.display())))?;
        }

        let url = source.url.as_str();
        let local = match url.strip_prefix("file://") {
            Some(p) => Some(PathBuf::from(p)),
            None if url.starts_with('/') => Some(PathBuf::from(url)),
            None => None,
        };
        let remote = url.starts_with("https://") || url.starts_with("http://");
        if local.is_none() && !remote {
            return Err(BuildError::UnsupportedUrl(url.to_string()));
        }

        fs::create_dir_all(&self.dir).map_err(io_ctx(format!("creating {}", self.dir.display())))?;
        let tmp = self.dir.join(format!(".partial-{}", std::process::id()));
        let _ = fs::remove_file(&tmp);
        let fetched = match &local {
            Some(src) => fs::copy(src, &tmp)
                .map(drop)
                .map_err(io_ctx(format!("copying {}", src.display()))),
            None => download(url, &tmp),
        };
        let verified = fetched.and_then(|()| hash_file(&tmp).map_err(io_ctx(format!("reading {}", tmp.display()))));
        let actual = match verified {
            Ok(a) => a,
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                return Err(e);
            }
        };
        if actual != expected {
            let _ = fs::remove_file(&tmp);
            return Err(BuildError::SourceHashMismatch { url: url.to_string(), expected, actual });
        }
        fs::rename(&tmp, &cached).map_err(io_ctx(format!("saving {}", cached.display())))?;
        Ok(cached)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;

    fn sha(bytes: &[u8]) -> String {
        hex(&Sha256::digest(bytes))
    }

    fn source(url: &str, content: &[u8]) -> Source {
        Source { url: url.into(), sha256: sha(content) }
    }

    fn have_curl() -> bool {
        Command::new("curl").arg("--version").output().is_ok_and(|o| o.status.success())
    }

    /// Serve `response` (a complete HTTP response) to the next `n` connections.
    fn serve(response: Vec<u8>, n: usize) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            for _ in 0..n {
                let (mut conn, _) = listener.accept().unwrap();
                let mut req = [0u8; 4096];
                let _ = conn.read(&mut req);
                conn.write_all(&response).unwrap();
            }
        });
        (format!("http://{addr}"), handle)
    }

    fn ok_response(body: &[u8]) -> Vec<u8> {
        let mut r = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).into_bytes();
        r.extend_from_slice(body);
        r
    }

    #[test]
    fn derives_safe_file_names() {
        assert_eq!(file_name_of("https://x.org/dl/foo-1.2.tar.gz").unwrap(), "foo-1.2.tar.gz");
        assert_eq!(file_name_of("file:///srv/a.tar.xz?download=1#frag").unwrap(), "a.tar.xz");
        for bad in ["https://x.org/", "https://x.org/..", "https://x.org/a b", "https://x.org/.hidden", ""] {
            assert!(file_name_of(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn copies_a_local_file_into_the_cache_after_verifying_it() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("pkg-1.0.tar.gz");
        fs::write(&src, b"tarball bytes").unwrap();
        let cache = SourceCache::new(d.path().join("cache"));
        let s = source(&format!("file://{}", src.display()), b"tarball bytes");
        let got = cache.fetch(&s).unwrap();
        assert_eq!(got, cache.path_for(&s).unwrap());
        assert_eq!(fs::read(&got).unwrap(), b"tarball bytes");
        assert!(got.file_name().unwrap().to_str().unwrap().starts_with(&s.sha256));
    }

    #[test]
    fn a_wrong_checksum_is_rejected_and_nothing_is_cached() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("pkg.tar.gz");
        fs::write(&src, b"actual").unwrap();
        let cache = SourceCache::new(d.path().join("cache"));
        let s = source(src.to_str().unwrap(), b"something else");
        let err = cache.fetch(&s).unwrap_err();
        assert!(matches!(err, BuildError::SourceHashMismatch { .. }), "{err}");
        assert_eq!(fs::read_dir(d.path().join("cache")).unwrap().count(), 0, "no partial file left");
    }

    #[test]
    fn a_cache_hit_needs_no_network_and_a_corrupt_entry_is_not_trusted() {
        let d = tempfile::tempdir().unwrap();
        let cache = SourceCache::new(d.path().join("cache"));
        // ftp is unsupported, so any attempt to fetch (instead of using the cache) fails.
        let s = source("ftp://example.invalid/pkg-1.0.tar.gz", b"good");
        fs::create_dir_all(d.path().join("cache")).unwrap();
        fs::write(cache.path_for(&s).unwrap(), b"good").unwrap();
        assert!(cache.fetch(&s).is_ok(), "served from the cache without any download");

        fs::write(cache.path_for(&s).unwrap(), b"corrupted").unwrap();
        let err = cache.fetch(&s).unwrap_err();
        assert!(matches!(err, BuildError::UnsupportedUrl(_)), "{err}");
        assert!(!cache.path_for(&s).unwrap().exists(), "the corrupt entry was discarded");
    }

    #[test]
    fn unsupported_url_schemes_are_refused() {
        let d = tempfile::tempdir().unwrap();
        let cache = SourceCache::new(d.path());
        for url in ["ftp://x.org/a.tar.gz", "git://x.org/a.tar.gz", "relative/a.tar.gz", "sftp://x/a.tar.gz"] {
            let err = cache.fetch(&source(url, b"x")).unwrap_err();
            assert!(matches!(err, BuildError::UnsupportedUrl(_)), "{url}: {err}");
        }
    }

    #[test]
    fn checksums_are_compared_case_insensitively() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("a.tar");
        fs::write(&src, b"x").unwrap();
        let mut s = source(src.to_str().unwrap(), b"x");
        s.sha256 = s.sha256.to_ascii_uppercase();
        assert!(SourceCache::new(d.path().join("c")).fetch(&s).is_ok());
    }

    #[test]
    fn downloads_over_http_and_verifies_the_checksum() {
        if !have_curl() {
            eprintln!("skipping: curl is not installed");
            return;
        }
        let body = b"downloaded tarball bytes".to_vec();
        let (base, server) = serve(ok_response(&body), 1);
        let d = tempfile::tempdir().unwrap();
        let cache = SourceCache::new(d.path().join("cache"));
        let s = source(&format!("{base}/dist/pkg-1.0.tar.gz"), &body);
        let got = cache.fetch(&s).unwrap();
        assert_eq!(fs::read(&got).unwrap(), body);
        server.join().unwrap();
        // The second fetch is a cache hit: the server is gone, so a download would fail.
        assert_eq!(cache.fetch(&s).unwrap(), got);
    }

    #[test]
    fn a_download_that_does_not_match_the_pinned_checksum_is_discarded() {
        if !have_curl() {
            eprintln!("skipping: curl is not installed");
            return;
        }
        let (base, server) = serve(ok_response(b"a tampered or substituted file"), 1);
        let d = tempfile::tempdir().unwrap();
        let cache = SourceCache::new(d.path().join("cache"));
        let s = source(&format!("{base}/pkg-1.0.tar.gz"), b"the file the recipe pinned");
        let err = cache.fetch(&s).unwrap_err();
        assert!(matches!(err, BuildError::SourceHashMismatch { .. }), "{err}");
        server.join().unwrap();
        assert_eq!(fs::read_dir(d.path().join("cache")).unwrap().count(), 0, "nothing was cached");
    }

    #[test]
    fn http_errors_are_reported_and_nothing_is_cached() {
        if !have_curl() {
            eprintln!("skipping: curl is not installed");
            return;
        }
        let not_found = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec();
        let (base, server) = serve(not_found, 1);
        let d = tempfile::tempdir().unwrap();
        let cache = SourceCache::new(d.path().join("cache"));
        let err = cache.fetch(&source(&format!("{base}/missing-1.0.tar.gz"), b"x")).unwrap_err();
        assert!(matches!(&err, BuildError::Download { message, .. } if message.contains("404")), "{err}");
        server.join().unwrap();
        assert_eq!(fs::read_dir(d.path().join("cache")).unwrap().count(), 0);
    }

    #[test]
    fn a_redirect_from_http_to_a_non_http_protocol_is_refused() {
        if !have_curl() {
            eprintln!("skipping: curl is not installed");
            return;
        }
        let redirect = b"HTTP/1.1 302 Found\r\nLocation: file:///etc/passwd\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec();
        let (base, server) = serve(redirect, 1);
        let d = tempfile::tempdir().unwrap();
        let cache = SourceCache::new(d.path().join("cache"));
        let err = cache.fetch(&source(&format!("{base}/pkg-1.0.tar.gz"), b"x")).unwrap_err();
        assert!(matches!(err, BuildError::Download { .. }), "{err}");
        server.join().unwrap();
    }
}

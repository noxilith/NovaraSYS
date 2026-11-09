//! Finding kernels on kernel.org and checking what was downloaded.

use serde::Deserialize;

use crate::KernelError;

pub const RELEASES_URL: &str = "https://www.kernel.org/releases.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// `mainline`, `stable`, `longterm` or `linux-next`.
    pub moniker: String,
    pub version: String,
}

#[derive(Deserialize)]
struct Raw {
    releases: Vec<RawRelease>,
}

#[derive(Deserialize)]
struct RawRelease {
    moniker: String,
    version: String,
}

/// Parse kernel.org's `releases.json`.
pub fn parse_releases(json: &str) -> Result<Vec<Release>, KernelError> {
    let raw: Raw = serde_json::from_str(json).map_err(|e| KernelError::Releases(e.to_string()))?;
    Ok(raw.releases.into_iter().map(|r| Release { moniker: r.moniker, version: r.version }).collect())
}

/// Which kernel to build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selector {
    /// Exactly this version, e.g. `7.3.3`.
    Version(String),
    /// The newest stable release.
    Stable,
    /// The newest long-term release (what to pick for a server or a system you do not touch often).
    Longterm,
}

/// A release candidate (`-rc`) is not something to install; so is anything that is not a plain version.
fn is_plain(v: &str) -> bool {
    !v.is_empty() && v.split('.').all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

/// Compare dotted versions numerically (7.10.1 > 7.9.4).
pub fn version_key(v: &str) -> Vec<u64> {
    v.split('.').map(|p| p.parse().unwrap_or(0)).collect()
}

/// The version `sel` stands for. An explicit version is only checked for shape, so an older release that
/// is no longer listed (but still on the server) can be built.
pub fn pick(releases: &[Release], sel: &Selector) -> Result<String, KernelError> {
    match sel {
        Selector::Version(v) => {
            if is_plain(v) {
                Ok(v.clone())
            } else {
                Err(KernelError::BadVersion(v.clone()))
            }
        }
        Selector::Stable | Selector::Longterm => {
            let want = if *sel == Selector::Stable { "stable" } else { "longterm" };
            releases
                .iter()
                .filter(|r| r.moniker == want && is_plain(&r.version))
                .max_by_key(|r| version_key(&r.version))
                .map(|r| r.version.clone())
                .ok_or(KernelError::NoRelease(want))
        }
    }
}

fn major_dir(version: &str) -> String {
    format!("v{}.x", version.split('.').next().unwrap_or("0"))
}

pub fn tarball_name(version: &str) -> String {
    format!("linux-{version}.tar.xz")
}

pub fn tarball_url(version: &str) -> String {
    format!("https://cdn.kernel.org/pub/linux/kernel/{}/{}", major_dir(version), tarball_name(version))
}

pub fn sums_url(version: &str) -> String {
    format!("https://cdn.kernel.org/pub/linux/kernel/{}/sha256sums.asc", major_dir(version))
}

/// The SHA-256 kernel.org lists for `file` in its `sha256sums.asc`. (That file is PGP-clearsigned; the
/// signature is not checked here, only the digest, so this protects against a corrupt or truncated
/// download and a tampered mirror, but not against kernel.org itself being compromised.)
pub fn parse_sums(text: &str, file: &str) -> Option<String> {
    text.lines().find_map(|l| {
        let mut it = l.split_whitespace();
        let (hash, name) = (it.next()?, it.next()?);
        (name == file && hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()))
            .then(|| hash.to_ascii_lowercase())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const JSON: &str = r#"{"latest_stable":{"version":"7.3.3"},"releases":[
        {"moniker":"mainline","version":"7.4-rc2"},
        {"moniker":"stable","version":"7.3.3"},
        {"moniker":"stable","version":"7.2.15"},
        {"moniker":"longterm","version":"6.18.62"},
        {"moniker":"longterm","version":"6.12.116"},
        {"moniker":"longterm","version":"6.6.162"},
        {"moniker":"linux-next","version":"next-20261109"}]}"#;

    #[test]
    fn picks_the_newest_stable_and_longterm_numerically() {
        let r = parse_releases(JSON).unwrap();
        assert_eq!(pick(&r, &Selector::Stable).unwrap(), "7.3.3");
        assert_eq!(pick(&r, &Selector::Longterm).unwrap(), "6.18.62", "6.18 is newer than 6.6, though 6.6 > 6.1 as text");
        assert_eq!(pick(&r, &Selector::Version("6.12.5".into())).unwrap(), "6.12.5");
    }

    #[test]
    fn release_candidates_and_odd_versions_are_never_picked_or_accepted() {
        let r = vec![Release { moniker: "stable".into(), version: "7.4-rc2".into() }];
        assert!(matches!(pick(&r, &Selector::Stable), Err(KernelError::NoRelease(_))));
        for bad in ["7.4-rc2", "", "7..1", "../../x", "7.3.3 ", "next-1"] {
            assert!(matches!(pick(&r, &Selector::Version(bad.into())), Err(KernelError::BadVersion(_))), "{bad:?}");
        }
    }

    #[test]
    fn urls_follow_kernel_orgs_layout() {
        assert_eq!(tarball_url("7.3.3"), "https://cdn.kernel.org/pub/linux/kernel/v7.x/linux-7.3.3.tar.xz");
        assert_eq!(tarball_url("6.12.116"), "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-6.12.116.tar.xz");
        assert_eq!(sums_url("7.3.3"), "https://cdn.kernel.org/pub/linux/kernel/v7.x/sha256sums.asc");
    }

    #[test]
    fn finds_the_digest_of_the_right_file() {
        let sums = "-----BEGIN PGP SIGNED MESSAGE-----\nHash: SHA256\n\n\
            aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11aa11  linux-7.3.2.tar.gz\n\
            79c66dde66cd96877fcde8f7d36aec946b1f423d2b97b5a7b1751b75c764f3d1  linux-7.3.3.tar.xz\n\
            -----BEGIN PGP SIGNATURE-----\n";
        assert_eq!(parse_sums(sums, "linux-7.3.3.tar.xz").unwrap(), "79c66dde66cd96877fcde8f7d36aec946b1f423d2b97b5a7b1751b75c764f3d1");
        assert!(parse_sums(sums, "linux-7.3.4.tar.xz").is_none());
        assert!(parse_sums("zz  linux-7.3.3.tar.xz\n", "linux-7.3.3.tar.xz").is_none(), "not a digest");
    }

    #[test]
    fn broken_json_is_an_error() {
        assert!(parse_releases("not json").is_err());
    }
}

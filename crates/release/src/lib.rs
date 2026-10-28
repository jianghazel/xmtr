use std::fs;
#[cfg(windows)]
use std::io::Cursor;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};
use ureq::Agent;

/// Target triple this crate was compiled for; release assets are named after it.
pub const TARGET: &str = env!("XMTR_TARGET");

#[cfg(windows)]
pub mod windows_path;

const DEFAULT_RELEASES: &str = "https://github.com/Alice39s/xmtr/releases";
const MAX_ASSET_BYTES: u64 = 64 * 1024 * 1024;
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);

/// Release page base URL; `XMTR_RELEASES` points the installer and `xmtr self update` at a mirror.
pub fn releases_url() -> String {
    std::env::var("XMTR_RELEASES")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map_or_else(
            || DEFAULT_RELEASES.to_owned(),
            |value| value.trim().trim_end_matches('/').to_owned(),
        )
}

pub const fn exe_name() -> &'static str {
    if cfg!(windows) { "xmtr.exe" } else { "xmtr" }
}

pub fn archive_name() -> String {
    if cfg!(windows) {
        format!("xmtr-{TARGET}.zip")
    } else {
        format!("xmtr-{TARGET}.tar.gz")
    }
}

pub fn agent(user_agent: &str) -> Agent {
    // Downloads go over HTTPS unless a mirror is explicitly configured with an http:// URL.
    let https_only = !releases_url().starts_with("http://");
    Agent::config_builder()
        .timeout_global(Some(DOWNLOAD_TIMEOUT))
        .https_only(https_only)
        .user_agent(user_agent)
        .build()
        .into()
}

/// Release tags carry a `v` prefix; `0.2.0` and `v0.2.0` name the same release.
pub fn normalize_tag(tag: &str) -> String {
    if tag.starts_with(|c: char| c.is_ascii_digit()) {
        format!("v{tag}")
    } else {
        tag.to_owned()
    }
}

/// Tag of the newest release, read from the redirect of `<releases>/latest`.
pub fn latest_tag(agent: &Agent) -> Result<String> {
    let url = format!("{}/latest", releases_url());
    let response = agent
        .get(&url)
        .config()
        .max_redirects(0)
        .http_status_as_error(false)
        .build()
        .call()
        .with_context(|| format!("requesting {url}"))?;
    let location = response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| {
            anyhow!(
                "{url} answered HTTP {} without a redirect to a release",
                response.status().as_u16()
            )
        })?;
    let tag = location
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|tag| !tag.is_empty() && *tag != "releases")
        .ok_or_else(|| anyhow!("{url} redirected to {location}, which names no release"))?;
    Ok(tag.to_owned())
}

pub fn is_newer(tag: &str, current: &str) -> Result<bool> {
    let parse = |value: &str| {
        semver::Version::parse(value.trim_start_matches('v'))
            .with_context(|| format!("{value} is not a version"))
    };
    Ok(parse(tag)? > parse(current)?)
}

fn download(agent: &Agent, url: &str) -> Result<Vec<u8>> {
    agent
        .get(url)
        .call()
        .with_context(|| format!("downloading {url}"))?
        .body_mut()
        .with_config()
        .limit(MAX_ASSET_BYTES)
        .read_to_vec()
        .with_context(|| format!("reading {url}"))
}

/// SHA-256 recorded for `file` in a `sha256sum` listing.
fn expected_sha256<'a>(sums: &'a str, file: &str) -> Result<&'a str> {
    sums.lines()
        .find_map(|line| {
            let (hash, name) = line.split_once(char::is_whitespace)?;
            (name.trim_start().trim_start_matches('*') == file).then_some(hash)
        })
        .ok_or_else(|| anyhow!("SHA256SUMS has no entry for {file}"))
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Downloads `file` from release `tag` and checks it against the release's SHA256SUMS.
pub fn fetch_verified(agent: &Agent, tag: &str, file: &str) -> Result<Vec<u8>> {
    let base = format!("{}/download/{tag}", releases_url());
    let sums = String::from_utf8(download(agent, &format!("{base}/SHA256SUMS"))?)
        .context("SHA256SUMS is not UTF-8")?;
    let expected = expected_sha256(&sums, file)?.to_ascii_lowercase();
    let bytes = download(agent, &format!("{base}/{file}"))?;
    let actual = sha256_hex(&bytes);
    if actual != expected {
        bail!("{file} does not match SHA256SUMS: expected {expected}, got {actual}");
    }
    Ok(bytes)
}

/// The xmtr executable inside a release archive.
fn read_binary(entry: impl Read, size: u64) -> Result<Vec<u8>> {
    if size > MAX_ASSET_BYTES {
        bail!(
            "{} exceeds the {MAX_ASSET_BYTES}-byte binary limit",
            exe_name()
        );
    }
    let mut binary = Vec::new();
    entry
        .take(MAX_ASSET_BYTES + 1)
        .read_to_end(&mut binary)
        .context("reading the xmtr binary")?;
    if binary.len() as u64 > MAX_ASSET_BYTES {
        bail!(
            "{} exceeds the {MAX_ASSET_BYTES}-byte binary limit",
            exe_name()
        );
    }
    Ok(binary)
}

/// The xmtr executable inside a release archive.
#[cfg(windows)]
pub fn extract_binary(archive: &[u8]) -> Result<Vec<u8>> {
    let mut zip = zip::ZipArchive::new(Cursor::new(archive)).context("opening the zip archive")?;
    let entry = zip
        .by_name(exe_name())
        .with_context(|| format!("the archive has no {}", exe_name()))?;
    let size = entry.size();
    read_binary(entry, size)
}

#[cfg(not(windows))]
pub fn extract_binary(archive: &[u8]) -> Result<Vec<u8>> {
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    for entry in tar.entries().context("reading the tar archive")? {
        let entry = entry?;
        if entry
            .path()?
            .file_name()
            .is_some_and(|name| name == exe_name())
        {
            let size = entry.size();
            return read_binary(entry, size);
        }
    }
    bail!("the archive has no {}", exe_name())
}

/// Downloads, verifies and unpacks the xmtr binary of release `tag` for this target.
pub fn fetch_binary(agent: &Agent, tag: &str) -> Result<Vec<u8>> {
    extract_binary(&fetch_verified(agent, tag, &archive_name())?)
}

/// Writes `bytes` to a new executable file next to `destination` and returns its path. The file name is
/// unique and must not exist yet, the data is flushed to disk, and a partial file is removed on error.
pub fn write_staged(destination: &Path, bytes: &[u8]) -> Result<PathBuf> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    static STAGED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let serial = STAGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let staged =
        destination.with_extension(format!("new-{}-{nanos:08x}-{serial}", std::process::id()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o755);
    }
    let written = options.open(&staged).and_then(|mut file| {
        file.write_all(bytes)?;
        file.sync_all()
    });
    if let Err(error) = written {
        let _ = fs::remove_file(&staged);
        return Err(error).with_context(|| format!("writing {}", staged.display()));
    }
    Ok(staged)
}

/// Fails early, before any download, when files cannot be created in `dir`.
pub fn ensure_writable(dir: &Path) -> Result<()> {
    let probe = write_staged(&dir.join(".xmtr-write-check"), &[])?;
    fs::remove_file(&probe).with_context(|| format!("removing {}", probe.display()))
}

/// PATH entry lists, kept free of registry access so the add and remove rules can be tested anywhere.
pub mod path_list {
    /// Expands `%NAME%` references the way Windows does for REG_EXPAND_SZ values; unknown names stay as is.
    pub fn expand(entry: &str, lookup: impl Fn(&str) -> Option<String>) -> String {
        let mut out = String::new();
        let mut rest = entry;
        while let Some(start) = rest.find('%') {
            let Some(len) = rest[start + 1..].find('%') else {
                break;
            };
            let name = &rest[start + 1..start + 1 + len];
            out.push_str(&rest[..start]);
            match lookup(name) {
                Some(value) if !name.is_empty() => out.push_str(&value),
                _ => out.push_str(&rest[start..start + len + 2]),
            }
            rest = &rest[start + len + 2..];
        }
        out.push_str(rest);
        out
    }

    fn same_dir(entry: &str, dir: &str, lookup: &impl Fn(&str) -> Option<String>) -> bool {
        expand(entry, lookup)
            .trim_end_matches('\\')
            .eq_ignore_ascii_case(dir.trim_end_matches('\\'))
    }

    /// `entries` plus `dir`, or `None` when an entry already names `dir`.
    pub fn with_dir(
        entries: &[&str],
        dir: &str,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Option<Vec<String>> {
        if entries.iter().any(|entry| same_dir(entry, dir, &lookup)) {
            return None;
        }
        let mut updated: Vec<String> = entries.iter().map(|entry| (*entry).to_owned()).collect();
        updated.push(dir.to_owned());
        Some(updated)
    }

    /// `entries` without those naming `dir`, or `None` when none did.
    pub fn without_dir(
        entries: &[&str],
        dir: &str,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Option<Vec<String>> {
        let kept: Vec<String> = entries
            .iter()
            .filter(|entry| !same_dir(entry, dir, &lookup))
            .map(|entry| (*entry).to_owned())
            .collect();
        (kept.len() != entries.len()).then_some(kept)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_sha256sum_listings() {
        let sums = "abc  xmtr-x86_64-unknown-linux-musl.tar.gz\ndef *xmtr-installer-x86_64-pc-windows-gnu.exe\n";
        assert_eq!(
            expected_sha256(sums, "xmtr-x86_64-unknown-linux-musl.tar.gz").unwrap(),
            "abc"
        );
        assert_eq!(
            expected_sha256(sums, "xmtr-installer-x86_64-pc-windows-gnu.exe").unwrap(),
            "def"
        );
        assert!(expected_sha256(sums, "xmtr-aarch64-apple-darwin.tar.gz").is_err());
    }

    #[test]
    fn compares_versions() {
        assert!(is_newer("v0.2.1", "0.2.0").unwrap());
        assert!(!is_newer("v0.2.0", "0.2.0").unwrap());
        assert!(!is_newer("v0.1.9", "0.2.0").unwrap());
        assert_eq!(normalize_tag("0.2.0"), "v0.2.0");
        assert_eq!(normalize_tag("v0.2.0"), "v0.2.0");
    }

    #[test]
    fn edits_path_lists_with_expanded_entries() {
        let lookup =
            |name: &str| (name == "LOCALAPPDATA").then(|| r"C:\Users\a\AppData\Local".to_owned());
        let dir = r"C:\Users\a\AppData\Local\Programs\xmtr";
        assert_eq!(
            path_list::expand(r"%LOCALAPPDATA%\Programs\xmtr", lookup),
            dir
        );
        assert_eq!(
            path_list::expand("%UNKNOWN%\\bin;50%", lookup),
            "%UNKNOWN%\\bin;50%"
        );
        assert_eq!(
            path_list::with_dir(&[r"%LOCALAPPDATA%\Programs\xmtr\"], dir, lookup),
            None
        );
        assert_eq!(
            path_list::with_dir(&[r"C:\bin"], dir, lookup),
            Some(vec![r"C:\bin".to_owned(), dir.to_owned()])
        );
        assert_eq!(
            path_list::without_dir(&[r"C:\bin", r"%LOCALAPPDATA%\programs\XMTR"], dir, lookup),
            Some(vec![r"C:\bin".to_owned()])
        );
        assert_eq!(path_list::without_dir(&[r"C:\bin"], dir, lookup), None);
    }

    #[test]
    fn stages_files_without_reusing_names() {
        let dir = Path::new(env!("OUT_DIR")).join(format!("stage-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let first = write_staged(&dir.join("xmtr"), b"one").unwrap();
        let second = write_staged(&dir.join("xmtr"), b"two").unwrap();
        assert_ne!(first, second);
        assert_eq!(fs::read(&first).unwrap(), b"one");
        ensure_writable(&dir).unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn hashes_like_sha256sum() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[cfg(not(windows))]
    fn binary_archive(size: u64) -> Vec<u8> {
        let gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let mut archive = tar::Builder::new(gzip);
        let mut header = tar::Header::new_gnu();
        header.set_size(size);
        header.set_mode(0o755);
        header.set_cksum();
        archive
            .append_data(&mut header, exe_name(), std::io::repeat(0).take(size))
            .unwrap();
        archive.into_inner().unwrap().finish().unwrap()
    }

    #[cfg(windows)]
    fn binary_archive(size: u64) -> Vec<u8> {
        let mut archive = zip::ZipWriter::new(Cursor::new(Vec::new()));
        archive
            .start_file(
                exe_name(),
                zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Deflated),
            )
            .unwrap();
        std::io::copy(&mut std::io::repeat(0).take(size), &mut archive).unwrap();
        archive.finish().unwrap().into_inner()
    }

    #[test]
    fn extracts_a_small_binary() {
        assert_eq!(extract_binary(&binary_archive(32)).unwrap(), vec![0; 32]);
    }

    #[test]
    fn rejects_a_binary_larger_than_the_download_limit() {
        let archive = binary_archive(MAX_ASSET_BYTES + 1);
        assert!(archive.len() < 1024 * 1024);
        let Err(error) = extract_binary(&archive) else {
            panic!("accepted an oversized binary");
        };
        assert!(error.to_string().contains("exceeds"), "{error:#}");
    }

    #[test]
    fn accepts_a_binary_at_the_limit() {
        let archive = binary_archive(MAX_ASSET_BYTES);
        assert_eq!(
            extract_binary(&archive).unwrap().len() as u64,
            MAX_ASSET_BYTES
        );
    }

    #[test]
    fn bounds_the_reader_when_the_declared_size_is_smaller() {
        let error = read_binary(std::io::repeat(0), 0).unwrap_err();
        assert!(error.to_string().contains("exceeds"), "{error:#}");
    }
}

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};

const REPOSITORY: &str = "RyanStoffel/forge";
const RELEASE_TAG: &str = "edge";
pub const BUILD_REVISION: &str = match option_env!("FORGE_BUILD_SHA") {
    Some(revision) => revision,
    None => "dev",
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub tag: String,
    pub revision: String,
    pub download_url: String,
    pub checksum_url: String,
    pub page_url: String,
}

#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    target_commitish: String,
    html_url: String,
    assets: Vec<ApiAsset>,
}

#[derive(Deserialize)]
struct ApiAsset {
    name: String,
    browser_download_url: String,
}

/// Whether Forge should check the `edge` release feed at all. A local `dev`
/// build has no meaningful revision to compare against, so it stays quiet
/// unless a developer explicitly opts in with `FORGE_FORCE_UPDATE`.
///
/// This is intentionally independent of [`self_install_enabled`]: a
/// Homebrew-managed install still benefits from knowing a newer release
/// exists, even though Forge must never replace its own executable when
/// Homebrew owns it.
pub fn checks_enabled() -> bool {
    std::env::var_os("FORGE_FORCE_UPDATE").is_some() || BUILD_REVISION != "dev"
}

/// Whether Forge may download and replace its own running executable.
/// False for Homebrew-managed installs (`Cellar/forge`, `Caskroom`-installed
/// `Forge.app`): Homebrew owns those binaries' lifecycle, so Forge only
/// surfaces a read-only notice for them instead (see `checks_enabled`).
pub fn self_install_enabled() -> bool {
    if std::env::var_os("FORGE_FORCE_UPDATE").is_some() {
        return true;
    }
    BUILD_REVISION != "dev"
        && std::env::current_exe()
            .map(|path| !is_homebrew_install(&path))
            .unwrap_or(true)
}

fn is_homebrew_install(path: &Path) -> bool {
    let components = path
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .collect::<Vec<_>>();
    components
        .windows(2)
        .any(|pair| pair[0] == "Cellar" && pair[1] == "forge")
        || components.contains(&"Forge.app")
}

pub fn check() -> Result<Option<Release>> {
    if !checks_enabled() || std::env::var_os("FORGE_DISABLE_UPDATES").is_some() {
        return Ok(None);
    }

    let url = format!("https://api.github.com/repos/{REPOSITORY}/releases/tags/{RELEASE_TAG}");
    let response = ureq::get(&url)
        .set("Accept", "application/vnd.github+json")
        .set("User-Agent", "Forge-Updater")
        .call()
        .map_err(|error| anyhow!("checking GitHub Releases: {error}"))?;
    let release: ApiRelease = response
        .into_json()
        .context("decoding the GitHub release response")?;

    release_from_api(release, BUILD_REVISION)
}

pub fn install(release: &Release) -> Result<()> {
    if !self_install_enabled() {
        return Err(anyhow!(
            "Homebrew owns this installation; run `brew upgrade --cask forge-app` instead"
        ));
    }
    let expected_checksum = download_text(&release.checksum_url)?
        .split_whitespace()
        .next()
        .map(str::to_owned)
        .context("release checksum is empty")?;
    if expected_checksum.len() != 64 || !expected_checksum.chars().all(|ch| ch.is_ascii_hexdigit())
    {
        return Err(anyhow!("release checksum is not a SHA-256 digest"));
    }

    let current = std::env::current_exe().context("locating the running Forge executable")?;
    let parent = current
        .parent()
        .context("Forge executable has no parent directory")?;
    let temporary = parent.join(format!(".forge-update-{}", std::process::id()));
    download_file(&release.download_url, &temporary)?;

    let actual_checksum = sha256(&temporary)?;
    if !actual_checksum.eq_ignore_ascii_case(&expected_checksum) {
        let _ = fs::remove_file(&temporary);
        return Err(anyhow!(
            "downloaded update failed checksum verification (expected {expected_checksum}, got {actual_checksum})"
        ));
    }

    make_executable(&temporary)?;
    replace_executable(&current, &temporary)
}

pub fn restart() -> Result<()> {
    let executable = std::env::current_exe().context("locating updated Forge executable")?;
    Command::new(executable)
        .spawn()
        .context("restarting Forge after update")?;
    Ok(())
}

fn release_from_api(release: ApiRelease, current_revision: &str) -> Result<Option<Release>> {
    if revisions_match(&release.target_commitish, current_revision) {
        return Ok(None);
    }

    let binary_name = release_asset_name();
    let checksum_name = format!("{binary_name}.sha256");
    let download_url = release
        .assets
        .iter()
        .find(|asset| asset.name == binary_name)
        .map(|asset| asset.browser_download_url.clone())
        .with_context(|| format!("release is missing {binary_name}"))?;
    let checksum_url = release
        .assets
        .iter()
        .find(|asset| asset.name == checksum_name)
        .map(|asset| asset.browser_download_url.clone())
        .with_context(|| format!("release is missing {checksum_name}"))?;

    Ok(Some(Release {
        tag: release.tag_name,
        revision: release.target_commitish,
        download_url,
        checksum_url,
        page_url: release.html_url,
    }))
}

fn release_asset_name() -> String {
    format!("forge-{}-apple-darwin", std::env::consts::ARCH)
}

fn revisions_match(candidate: &str, current: &str) -> bool {
    candidate == current
        || (candidate.len() >= 7
            && current.len() >= 7
            && (candidate.starts_with(current) || current.starts_with(candidate)))
}

fn download_text(url: &str) -> Result<String> {
    ureq::get(url)
        .set("User-Agent", "Forge-Updater")
        .call()
        .map_err(|error| anyhow!("downloading update checksum: {error}"))?
        .into_string()
        .context("reading update checksum")
}

fn download_file(url: &str, destination: &Path) -> Result<()> {
    let response = ureq::get(url)
        .set("User-Agent", "Forge-Updater")
        .call()
        .map_err(|error| anyhow!("downloading update: {error}"))?;
    let mut reader = response.into_reader();
    let mut file = File::create(destination).context("creating temporary update file")?;
    std::io::copy(&mut reader, &mut file).context("writing downloaded update")?;
    file.flush().context("flushing downloaded update")?;
    file.sync_all().context("syncing downloaded update")?;
    if file.metadata()?.len() == 0 {
        let _ = fs::remove_file(destination);
        return Err(anyhow!("downloaded update is empty"));
    }
    Ok(())
}

fn sha256(path: &Path) -> Result<String> {
    let mut file = File::open(path).context("opening downloaded update for verification")?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).context("marking downloaded update executable")
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}

fn replace_executable(current: &Path, replacement: &Path) -> Result<()> {
    let backup = backup_path(current);
    if backup.exists() {
        fs::remove_file(&backup).context("removing the previous update backup")?;
    }
    fs::rename(current, &backup).context("backing up the current Forge executable")?;
    if let Err(error) = fs::rename(replacement, current) {
        let _ = fs::rename(&backup, current);
        return Err(error).context("installing the new Forge executable");
    }
    Ok(())
}

fn backup_path(current: &Path) -> PathBuf {
    current.with_file_name(".forge-previous")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api_release(revision: &str) -> ApiRelease {
        let binary = release_asset_name();
        ApiRelease {
            tag_name: "edge".into(),
            target_commitish: revision.into(),
            html_url: "https://github.com/RyanStoffel/forge/releases/tag/edge".into(),
            assets: vec![
                ApiAsset {
                    name: binary.clone(),
                    browser_download_url: "https://example.test/forge".into(),
                },
                ApiAsset {
                    name: format!("{binary}.sha256"),
                    browser_download_url: "https://example.test/forge.sha256".into(),
                },
            ],
        }
    }

    #[test]
    fn identical_revision_is_current() {
        assert!(release_from_api(api_release("abcdef123"), "abcdef123")
            .unwrap()
            .is_none());
    }

    #[test]
    fn abbreviated_revision_is_current() {
        assert!(release_from_api(api_release("abcdef1"), "abcdef123456789")
            .unwrap()
            .is_none());
    }

    #[test]
    fn different_revision_is_an_update() {
        let release = release_from_api(api_release("123456789"), "abcdef123")
            .unwrap()
            .unwrap();
        assert_eq!(release.tag, "edge");
        assert_eq!(release.revision, "123456789");
    }

    #[test]
    fn missing_checksum_rejects_release() {
        let mut release = api_release("123456789");
        release.assets.pop();
        assert!(release_from_api(release, "abcdef123").is_err());
    }

    #[test]
    fn install_refuses_to_run_when_self_install_is_disabled() {
        // Under `cargo test`, `FORGE_BUILD_SHA` is unset, so `BUILD_REVISION`
        // is "dev" and `self_install_enabled()` is false (the same state a
        // Homebrew-managed install reports). `install` must refuse before
        // touching the network or the filesystem.
        assert!(!self_install_enabled());
        let release = Release {
            tag: "edge".into(),
            revision: "123456789".into(),
            download_url: "https://example.test/forge".into(),
            checksum_url: "https://example.test/forge.sha256".into(),
            page_url: "https://github.com/RyanStoffel/forge/releases/tag/edge".into(),
        };
        assert!(install(&release).is_err());
    }

    #[test]
    fn detects_homebrew_cellar_installation() {
        assert!(is_homebrew_install(Path::new(
            "/opt/homebrew/Cellar/forge/0.1.0-edge.abcdef1/bin/forge"
        )));
        assert!(is_homebrew_install(Path::new(
            "/usr/local/Cellar/forge/0.1.0/bin/forge"
        )));
        assert!(is_homebrew_install(Path::new(
            "/Applications/Forge.app/Contents/MacOS/forge"
        )));
    }

    #[test]
    fn does_not_treat_direct_binary_as_homebrew_installation() {
        assert!(!is_homebrew_install(Path::new(
            "/Applications/Forge Preview/Contents/MacOS/forge"
        )));
        assert!(!is_homebrew_install(Path::new("/Users/test/bin/forge")));
    }
}

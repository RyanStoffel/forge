//! Native GitHub sign-in.
//!
//! Forge authenticates with GitHub's OAuth Device Authorization Grant
//! (<https://docs.github.com/apps/oauth-apps/building-oauth-apps/authorizing-oauth-apps#device-flow>)
//! directly — no `gh` CLI in the critical path. The device flow is the
//! correct grant for a native app with no backend to hold a client secret:
//! the client id below is a public identifier for Forge's registered OAuth
//! App (device flow enabled, no secret issued), safe to embed in source,
//! exactly like the `gh` CLI's own client id.
//!
//! The resulting token lives only in the macOS Keychain. Git picks it up
//! through a credential helper Forge registers for `github.com`: Git invokes
//! this same binary as `forge git-credential <op>`, which answers `get`
//! requests from the Keychain and no-ops `store`/`erase`, since Forge (not
//! Git) owns the credential's lifecycle.

use std::io::Read;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

/// Forge's registered GitHub OAuth App id (device flow, public client).
const CLIENT_ID: &str = "Ov23lisqZZbdfPbCZM0w";
/// `repo` authenticates Git operations through the credential helper;
/// `read:user` is enough for the sidebar/profile identity (login, name,
/// avatar). Expand deliberately if a feature needs more.
const SCOPES: &str = "repo read:user";
const KEYCHAIN_SERVICE: &str = "Forge: GitHub OAuth Token";
const KEYCHAIN_ACCOUNT: &str = "github.com";
/// `errSecItemNotFound` (`Security/SecBase.h`) — the OSStatus Keychain APIs
/// return when no matching item exists.
const KEYCHAIN_ITEM_NOT_FOUND: i32 = -25300;
const GIT_CREDENTIAL_KEY: &str = "credential.https://github.com.helper";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    pub login: String,
    pub name: Option<String>,
    pub avatar_url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AccountLookup {
    Connected(Account),
    SignedOut,
    Failed(String),
}

/// A pending device-flow authorization: the code Forge shows the user and
/// what it needs to keep polling GitHub for the outcome.
#[derive(Clone, Debug)]
pub struct DeviceAuthorization {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub interval: Duration,
    pub expires_at: Instant,
}

#[derive(Debug)]
pub enum DevicePoll {
    Authorized(String),
    Pending,
    SlowDown,
}

#[derive(Deserialize, Default)]
struct DeviceCodeResponse {
    device_code: Option<String>,
    user_code: Option<String>,
    verification_uri: Option<String>,
    expires_in: Option<u64>,
    interval: Option<u64>,
    error: Option<String>,
    error_description: Option<String>,
}

#[derive(Deserialize, Default)]
struct TokenResponse {
    access_token: Option<String>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct ApiUser {
    login: String,
    name: Option<String>,
    avatar_url: Option<String>,
}

/// Step 1 of the device flow: ask GitHub for a device/user code pair.
pub fn request_device_authorization() -> Result<DeviceAuthorization> {
    let response = ureq::post("https://github.com/login/device/code")
        .set("Accept", "application/json")
        .send_form(&[("client_id", CLIENT_ID), ("scope", SCOPES)])
        .map_err(|error| anyhow!("requesting a GitHub device code: {error}"))?;
    let body: DeviceCodeResponse = response
        .into_json()
        .context("decoding the GitHub device code response")?;
    let (device_code, user_code, verification_uri, interval, expires_in) =
        parse_device_code_response(body)?;
    Ok(DeviceAuthorization {
        device_code,
        user_code,
        verification_uri,
        interval: Duration::from_secs(interval),
        expires_at: Instant::now() + Duration::from_secs(expires_in),
    })
}

fn parse_device_code_response(
    body: DeviceCodeResponse,
) -> Result<(String, String, String, u64, u64)> {
    if let Some(error) = body.error {
        let description = body.error_description.unwrap_or(error);
        return Err(anyhow!(
            "GitHub declined the device code request: {description}"
        ));
    }
    let device_code = body.device_code.context("GitHub omitted a device code")?;
    let user_code = body.user_code.context("GitHub omitted a one-time code")?;
    let verification_uri = body
        .verification_uri
        .context("GitHub omitted a verification URL")?;
    Ok((
        device_code,
        user_code,
        verification_uri,
        body.interval.unwrap_or(5).max(5),
        body.expires_in.unwrap_or(900),
    ))
}

/// Opens the device verification page so the user only has to type the code.
pub fn open_verification_uri(uri: &str) {
    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("open").arg(uri).status();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = Command::new("xdg-open").arg(uri).status();
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = uri;
    }
}

/// Step 3 of the device flow: one poll attempt. Callers loop this on
/// `authorization_pending`, respecting `interval` (widening it on
/// `slow_down`), until it returns `Authorized`, an expiry, or a denial.
pub fn poll_device_token(device_code: &str) -> Result<DevicePoll> {
    let response = ureq::post("https://github.com/login/oauth/access_token")
        .set("Accept", "application/json")
        .send_form(&[
            ("client_id", CLIENT_ID),
            ("device_code", device_code),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ])
        .map_err(|error| anyhow!("checking GitHub sign-in status: {error}"))?;
    let body: TokenResponse = response
        .into_json()
        .context("decoding the GitHub sign-in response")?;
    interpret_token_response(body)
}

fn interpret_token_response(body: TokenResponse) -> Result<DevicePoll> {
    if let Some(token) = body.access_token {
        return Ok(DevicePoll::Authorized(token));
    }
    match body.error.as_deref() {
        Some("authorization_pending") => Ok(DevicePoll::Pending),
        Some("slow_down") => Ok(DevicePoll::SlowDown),
        Some("expired_token") => Err(anyhow!("The one-time code expired. Start sign-in again.")),
        Some("access_denied") => Err(anyhow!("Sign-in was cancelled from GitHub.")),
        Some(other) => Err(anyhow!("GitHub sign-in failed ({other}).")),
        None => Err(anyhow!(
            "GitHub sign-in failed with an unrecognized response."
        )),
    }
}

/// Step 4: exchange the access token for a profile, save it to the
/// Keychain, and point Git at Forge's credential helper for github.com.
pub fn complete_sign_in(token: &str) -> Result<Account> {
    let account = fetch_account(token)?;
    store_token(token)?;
    configure_git_credential_helper(true)?;
    Ok(account)
}

/// Reverses `complete_sign_in`: drops the Keychain entry and Forge's Git
/// credential helper registration.
pub fn sign_out() -> Result<()> {
    clear_token()?;
    configure_git_credential_helper(false)
}

/// Startup/refresh path: uses whatever token is already in the Keychain
/// rather than starting a new device flow.
pub fn lookup_account() -> Result<AccountLookup> {
    let Some(token) = load_token()? else {
        return Ok(AccountLookup::SignedOut);
    };
    match fetch_account(&token) {
        Ok(account) => Ok(AccountLookup::Connected(account)),
        Err(error) => {
            // The token is likely expired or revoked; drop it so the user
            // gets a clean "signed out" state instead of a stuck failure.
            let _ = clear_token();
            Ok(AccountLookup::Failed(error.to_string()))
        }
    }
}

fn fetch_account(token: &str) -> Result<Account> {
    let response = ureq::get("https://api.github.com/user")
        .set("Accept", "application/vnd.github+json")
        .set("Authorization", &format!("Bearer {token}"))
        .set("User-Agent", "Forge-App")
        .set("X-GitHub-Api-Version", "2022-11-28")
        .call()
        .map_err(|error| anyhow!("reading your GitHub profile: {error}"))?;
    let user: ApiUser = response
        .into_json()
        .context("decoding your GitHub profile")?;
    Ok(Account {
        login: user.login,
        name: user.name.filter(|name| !name.trim().is_empty()),
        avatar_url: user.avatar_url,
    })
}

/// Downloads the avatar image and returns it as `(content-type, bytes)`.
/// Kept UI-agnostic on purpose: this crate module never depends on GPUI, so
/// the caller decides how to turn bytes into a renderable image.
pub fn fetch_avatar_bytes(url: &str) -> Result<(String, Vec<u8>)> {
    let response = ureq::get(url)
        .set("User-Agent", "Forge-App")
        .call()
        .map_err(|error| anyhow!("downloading your GitHub avatar: {error}"))?;
    let content_type = response.content_type().to_string();
    let mut bytes = Vec::new();
    response
        .into_reader()
        .read_to_end(&mut bytes)
        .context("reading your GitHub avatar")?;
    Ok((content_type, bytes))
}

pub fn onboarding_completed() -> bool {
    onboarding_marker().is_some_and(|path| path.is_file())
}

pub fn complete_onboarding() -> Result<()> {
    let path = onboarding_marker().context("HOME is not available")?;
    let parent = path
        .parent()
        .context("onboarding marker has no parent directory")?;
    std::fs::create_dir_all(parent).context("creating Forge application data directory")?;
    std::fs::write(path, b"completed\n").context("saving onboarding state")
}

fn onboarding_marker() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join("Library/Application Support/Forge/onboarding-complete"))
}

/// Entry point for `forge git-credential <get|store|erase>`. Git invokes the
/// helper this way once `complete_sign_in` registers it. Returns `None` when
/// the process was not invoked as a credential helper, so `main` can fall
/// through to launching the app.
pub fn run_git_credential_helper() -> Option<i32> {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() != Some("git-credential") {
        return None;
    }
    let operation = args.next().unwrap_or_default();
    Some(match operation.as_str() {
        "get" => git_credential_get(),
        // Forge, not Git, owns this credential's lifecycle: acknowledge and
        // ignore both. This mirrors `gh auth git-credential`.
        "store" | "erase" => 0,
        other => {
            eprintln!("forge: unsupported git-credential operation `{other}`");
            1
        }
    })
}

fn git_credential_get() -> i32 {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return 1;
    }
    if requested_host(&input) != Some("github.com") {
        return 0;
    }
    match load_token() {
        Ok(Some(token)) => {
            println!("username=x-access-token");
            println!("password={token}");
            0
        }
        Ok(None) => 0,
        Err(error) => {
            eprintln!("forge: reading the stored GitHub token failed: {error:#}");
            1
        }
    }
}

fn requested_host(input: &str) -> Option<&str> {
    input.lines().find_map(|line| line.strip_prefix("host="))
}

fn configure_git_credential_helper(enable: bool) -> Result<()> {
    // Clear anything already configured for this exact scope first, so
    // Forge's helper is the sole answer for github.com and sign-out fully
    // reverses this — never touches unrelated global Git configuration.
    let _ = Command::new("git")
        .args(["config", "--global", "--unset-all", GIT_CREDENTIAL_KEY])
        .status();
    if !enable {
        return Ok(());
    }
    let status = Command::new("git")
        .args([
            "config",
            "--global",
            "--add",
            GIT_CREDENTIAL_KEY,
            "!forge git-credential",
        ])
        .status()
        .context("configuring Git to use Forge's GitHub credentials")?;
    if !status.success() {
        return Err(anyhow!("Git rejected the credential helper configuration"));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn store_token(token: &str) -> Result<()> {
    security_framework::passwords::set_generic_password(
        KEYCHAIN_SERVICE,
        KEYCHAIN_ACCOUNT,
        token.as_bytes(),
    )
    .map_err(|error| anyhow!("saving your GitHub token to the Keychain: {error:?}"))
}

#[cfg(target_os = "macos")]
fn load_token() -> Result<Option<String>> {
    match security_framework::passwords::get_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT) {
        Ok(bytes) => Ok(Some(
            String::from_utf8(bytes).context("decoding the stored GitHub token")?,
        )),
        Err(error) if error.code() == KEYCHAIN_ITEM_NOT_FOUND => Ok(None),
        Err(error) => Err(anyhow!(
            "reading your GitHub token from the Keychain: {error:?}"
        )),
    }
}

#[cfg(target_os = "macos")]
fn clear_token() -> Result<()> {
    match security_framework::passwords::delete_generic_password(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
    {
        Ok(()) => Ok(()),
        Err(error) if error.code() == KEYCHAIN_ITEM_NOT_FOUND => Ok(()),
        Err(error) => Err(anyhow!(
            "removing your GitHub token from the Keychain: {error:?}"
        )),
    }
}

#[cfg(not(target_os = "macos"))]
fn store_token(_token: &str) -> Result<()> {
    Err(anyhow!(
        "GitHub sign-in currently requires macOS Keychain support"
    ))
}

#[cfg(not(target_os = "macos"))]
fn load_token() -> Result<Option<String>> {
    Ok(None)
}

#[cfg(not(target_os = "macos"))]
fn clear_token() -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_device_code_response() {
        let body = DeviceCodeResponse {
            device_code: Some("dc".into()),
            user_code: Some("ABCD-1234".into()),
            verification_uri: Some("https://github.com/login/device".into()),
            expires_in: Some(900),
            interval: Some(5),
            error: None,
            error_description: None,
        };
        let (device_code, user_code, uri, interval, expires_in) =
            parse_device_code_response(body).unwrap();
        assert_eq!(device_code, "dc");
        assert_eq!(user_code, "ABCD-1234");
        assert_eq!(uri, "https://github.com/login/device");
        assert_eq!(interval, 5);
        assert_eq!(expires_in, 900);
    }

    #[test]
    fn device_code_interval_has_a_five_second_floor() {
        let body = DeviceCodeResponse {
            device_code: Some("dc".into()),
            user_code: Some("ABCD-1234".into()),
            verification_uri: Some("https://github.com/login/device".into()),
            interval: Some(1),
            ..Default::default()
        };
        let (.., interval, _) = parse_device_code_response(body).unwrap();
        assert_eq!(interval, 5);
    }

    #[test]
    fn device_code_error_is_reported() {
        let body = DeviceCodeResponse {
            error: Some("device_flow_disabled".into()),
            error_description: Some("the device flow is not enabled".into()),
            ..Default::default()
        };
        let error = parse_device_code_response(body).unwrap_err();
        assert!(error.to_string().contains("the device flow is not enabled"));
    }

    #[test]
    fn authorized_token_response_yields_the_token() {
        let body = TokenResponse {
            access_token: Some("gho_example".into()),
            error: None,
        };
        match interpret_token_response(body).unwrap() {
            DevicePoll::Authorized(token) => assert_eq!(token, "gho_example"),
            other => panic!("expected Authorized, got {other:?}"),
        }
    }

    #[test]
    fn authorization_pending_keeps_polling() {
        let body = TokenResponse {
            access_token: None,
            error: Some("authorization_pending".into()),
        };
        assert!(matches!(
            interpret_token_response(body).unwrap(),
            DevicePoll::Pending
        ));
    }

    #[test]
    fn slow_down_keeps_polling() {
        let body = TokenResponse {
            access_token: None,
            error: Some("slow_down".into()),
        };
        assert!(matches!(
            interpret_token_response(body).unwrap(),
            DevicePoll::SlowDown
        ));
    }

    #[test]
    fn expired_token_is_a_terminal_error() {
        let body = TokenResponse {
            access_token: None,
            error: Some("expired_token".into()),
        };
        assert!(interpret_token_response(body).is_err());
    }

    #[test]
    fn access_denied_is_a_terminal_error() {
        let body = TokenResponse {
            access_token: None,
            error: Some("access_denied".into()),
        };
        assert!(interpret_token_response(body).is_err());
    }

    #[test]
    fn parses_github_user_with_optional_fields() {
        let user: ApiUser = serde_json::from_str(
            r#"{"login":"octocat","name":"The Octocat","avatar_url":"https://example.test/a.png"}"#,
        )
        .unwrap();
        assert_eq!(user.login, "octocat");
        assert_eq!(user.name.as_deref(), Some("The Octocat"));
        assert_eq!(
            user.avatar_url.as_deref(),
            Some("https://example.test/a.png")
        );
    }

    #[test]
    fn parses_github_user_without_display_name() {
        let user: ApiUser =
            serde_json::from_str(r#"{"login":"octocat","name":null,"avatar_url":null}"#).unwrap();
        assert_eq!(user.login, "octocat");
        assert_eq!(user.name, None);
        assert_eq!(user.avatar_url, None);
    }

    #[test]
    fn extracts_requested_host_from_git_credential_input() {
        assert_eq!(
            requested_host("protocol=https\nhost=github.com\n"),
            Some("github.com")
        );
        assert_eq!(
            requested_host("protocol=https\nhost=gitlab.com\npath=x\n"),
            Some("gitlab.com")
        );
        assert_eq!(requested_host("protocol=https\n"), None);
    }
}

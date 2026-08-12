use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    pub login: String,
    pub name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AccountLookup {
    Connected(Account),
    SignedOut,
    Unavailable(String),
}

#[derive(Deserialize)]
struct ApiAccount {
    login: String,
    name: Option<String>,
}

pub fn lookup_account() -> Result<AccountLookup> {
    let output = match Command::new("gh").args(["api", "user"]).output() {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(AccountLookup::Unavailable(
                "Install the GitHub CLI (`brew install gh`) to connect your account.".into(),
            ));
        }
        Err(error) => return Err(error).context("launching GitHub CLI"),
    };

    if !output.status.success() {
        return Ok(AccountLookup::SignedOut);
    }

    let account: ApiAccount =
        serde_json::from_slice(&output.stdout).context("reading GitHub account details")?;
    Ok(AccountLookup::Connected(Account {
        login: account.login,
        name: account.name.filter(|name| !name.trim().is_empty()),
    }))
}

pub fn sign_in() -> Result<Account> {
    let status = Command::new("gh")
        .args([
            "auth",
            "login",
            "--hostname",
            "github.com",
            "--web",
            "--clipboard",
            "--git-protocol",
            "ssh",
            "--skip-ssh-key",
        ])
        .stdin(Stdio::null())
        .status()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                anyhow!("GitHub CLI is not installed. Install it with `brew install gh`.")
            } else {
                anyhow!(error)
            }
        })?;
    if !status.success() {
        return Err(anyhow!("GitHub sign-in did not complete"));
    }

    let setup = Command::new("gh")
        .args(["auth", "setup-git", "--hostname", "github.com"])
        .status()
        .context("configuring Git to use the GitHub account")?;
    if !setup.success() {
        return Err(anyhow!("GitHub connected, but Git credential setup failed"));
    }

    match lookup_account()? {
        AccountLookup::Connected(account) => Ok(account),
        _ => Err(anyhow!("GitHub sign-in finished without an active account")),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_github_account_with_optional_display_name() {
        let account: ApiAccount =
            serde_json::from_str(r#"{"login":"octocat","name":"The Octocat"}"#).unwrap();
        assert_eq!(account.login, "octocat");
        assert_eq!(account.name.as_deref(), Some("The Octocat"));
    }

    #[test]
    fn parses_github_account_without_display_name() {
        let account: ApiAccount =
            serde_json::from_str(r#"{"login":"octocat","name":null}"#).unwrap();
        assert_eq!(account.login, "octocat");
        assert_eq!(account.name, None);
    }
}

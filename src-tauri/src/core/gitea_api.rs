//! Minimal Gitea REST client for the guided backup setup, mirroring
//! `github_api`: validate a token, then find or create the private backup
//! repository. The token never appears in URLs, logs, or error messages —
//! callers store it in the OS keychain.
//!
//! Errors carry stable prefixes (`GITEA_TOKEN_INVALID`, `GITEA_SCOPE`,
//! `GITEA_NETWORK`) the frontend maps to plain-language copy.

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use super::skillssh_api::build_http_client;

/// Repository name rules are the conservative subset shared with GitHub
/// (`github_api::is_valid_repo_name`); every Gitea instance accepts them.
pub use super::github_api::is_valid_repo_name;

#[derive(Debug, Clone, serde::Serialize)]
pub struct GiteaConnectInfo {
    pub login: String,
    pub repo_full_name: String,
    /// Credential-free HTTPS clone URL.
    pub url: String,
    pub repo_created: bool,
    /// False when the user connected a pre-existing PUBLIC repository — the
    /// UI warns, since a public backup is almost never intentional.
    /// Repositories created by the app are always private.
    pub repo_private: bool,
}

#[derive(Deserialize)]
struct UserResp {
    login: String,
}

#[derive(Deserialize)]
struct RepoResp {
    full_name: String,
    /// Missing field is treated as private so a parsing gap can only ever
    /// suppress the warning, never raise a false alarm.
    private: Option<bool>,
}

/// Trim a user-supplied Gitea base URL to `scheme://host[/path]` with no
/// trailing slash. Rejects anything that is not http(s).
pub fn normalize_base_url(input: &str) -> Result<String> {
    let trimmed = input.trim().trim_end_matches('/');
    let lower = trimmed.to_lowercase();
    if !lower.starts_with("https://") && !lower.starts_with("http://") {
        bail!("GITEA_BAD_URL: the Gitea URL must start with https:// or http://");
    }
    let rest = &trimmed[trimmed.find("://").unwrap() + 3..];
    let host_part = rest.split(['/', '?', '#']).next().unwrap_or("");
    let no_host = host_part.is_empty()
        || (!host_part.contains('.') && !host_part.contains(':') && host_part != "localhost");
    if no_host {
        bail!("GITEA_BAD_URL: the Gitea URL has no host");
    }
    Ok(trimmed.to_string())
}

fn request(
    client: &reqwest::blocking::Client,
    method: reqwest::Method,
    url: &str,
    token: &str,
) -> reqwest::blocking::RequestBuilder {
    client
        .request(method, url)
        .header("Authorization", format!("token {token}"))
        .header("Accept", "application/json")
}

/// Validate the token, then ensure the backup repository exists under the
/// token owner's account (creating it as a private repo when missing).
pub fn connect_backup_repo(
    base_url: &str,
    token: &str,
    repo_name: &str,
    proxy_url: Option<&str>,
) -> Result<GiteaConnectInfo> {
    if !is_valid_repo_name(repo_name) {
        bail!("Invalid repository name");
    }
    let base = normalize_base_url(base_url)?;
    let client = build_http_client(proxy_url, 20);

    // Who owns this token? Also serves as token validation.
    let resp = request(
        &client,
        reqwest::Method::GET,
        &format!("{base}/api/v1/user"),
        token,
    )
    .send()
    .context("GITEA_NETWORK: could not reach the Gitea server")?;
    let login = match resp.status().as_u16() {
        200 => resp.json::<UserResp>().context("Unexpected /api/v1/user response")?.login,
        401 => bail!("GITEA_TOKEN_INVALID: Gitea rejected the token (401)"),
        403 => bail!("GITEA_TOKEN_INVALID: Gitea denied access (403); the token may lack permissions or be rate-limited"),
        s => bail!("Gitea /api/v1/user returned HTTP {s}"),
    };

    // Find or create the repository.
    let resp = request(
        &client,
        reqwest::Method::GET,
        &format!("{base}/api/v1/repos/{login}/{repo_name}"),
        token,
    )
    .send()
    .context("GITEA_NETWORK: could not reach the Gitea server")?;

    let (repo_created, repo) = match resp.status().as_u16() {
        200 => (
            false,
            resp.json::<RepoResp>()
                .context("Unexpected repo response")?,
        ),
        404 => {
            let resp = request(
                &client,
                reqwest::Method::POST,
                &format!("{base}/api/v1/user/repos"),
                token,
            )
            .json(&serde_json::json!({
                "name": repo_name,
                "private": true,
                "auto_init": false,
                "description": "Skills Manager backup",
            }))
            .send()
            .context("GITEA_NETWORK: could not reach the Gitea server")?;
            match resp.status().as_u16() {
                201 => (
                    true,
                    resp.json::<RepoResp>()
                        .context("Unexpected create-repo response")?,
                ),
                401 => bail!("GITEA_TOKEN_INVALID: Gitea rejected the token (401)"),
                403 | 404 => bail!(
                    "GITEA_SCOPE: the token cannot create repositories — grant it write access (scope: repository)"
                ),
                s => bail!("Gitea create-repo returned HTTP {s}"),
            }
        }
        401 => bail!("GITEA_TOKEN_INVALID: Gitea rejected the token (401)"),
        403 => bail!(
            "GITEA_SCOPE: the token cannot read this repository (403); grant it access to {login}/{repo_name}"
        ),
        s => bail!("Gitea repo lookup returned HTTP {s}"),
    };

    let full_name = repo.full_name;
    let repo_private = repo.private.unwrap_or(true);
    log::info!(
        "gitea connect: using repository {full_name} (created={repo_created}, private={repo_private})"
    );
    Ok(GiteaConnectInfo {
        login,
        url: format!("{base}/{full_name}.git"),
        repo_full_name: full_name,
        repo_created,
        repo_private,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_normalization() {
        assert_eq!(
            normalize_base_url("https://git.nxwysoft.com/").unwrap(),
            "https://git.nxwysoft.com"
        );
        assert_eq!(
            normalize_base_url("  https://gitea.example.com:8443  ").unwrap(),
            "https://gitea.example.com:8443"
        );
        assert_eq!(
            normalize_base_url("http://localhost:3000").unwrap(),
            "http://localhost:3000"
        );
        assert!(normalize_base_url("git.nxwysoft.com").is_err());
        assert!(normalize_base_url("ftp://git.nxwysoft.com").is_err());
        assert!(normalize_base_url("https://").is_err());
    }

    #[test]
    fn repo_name_validation_shared_with_github() {
        assert!(is_valid_repo_name("skills-manager-backup"));
        assert!(!is_valid_repo_name("has/slash"));
    }
}

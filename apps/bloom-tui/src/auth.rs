use anyhow::{bail, Context, Result};
use bloom_core::{Bloom, OsKeyring};
use sha2::{Digest, Sha256};
use std::{
    io::{self, Write},
    path::PathBuf,
    sync::Arc,
};
use url::Url;

pub fn normalize_homeserver(input: &str) -> Result<String> {
    let input = input.trim();
    let Some((scheme, remainder)) = input.split_once("://") else {
        bail!("Homeserver must be a valid http:// or https:// URL");
    };
    if !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https") {
        bail!("Homeserver must use http:// or https://");
    }
    let authority = remainder.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() || authority.contains('@') {
        bail!("Homeserver URL must include a host and must not contain credentials");
    }

    let mut url =
        Url::parse(input).context("Homeserver must be a valid http:// or https:// URL")?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("Homeserver must use http:// or https://");
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("Homeserver URL must not contain credentials");
    }
    if url.query().is_some() || url.fragment().is_some() {
        bail!("Homeserver URL must not contain a query or fragment");
    }
    if url.host().is_none() {
        bail!("Homeserver URL must include a host");
    }
    // URL parsing canonicalizes host casing, IDNs, and default ports. Keep any
    // proxy path prefix, but remove redundant trailing slashes.
    let path = url.path().trim_end_matches('/').to_owned();
    url.set_path(if path.is_empty() { "/" } else { &path });
    Ok(url.to_string().trim_end_matches('/').to_owned())
}

pub fn account_id(homeserver: &str, username: &str) -> Result<String> {
    let homeserver = normalize_homeserver(homeserver)?;
    let username = username.trim();
    if username.is_empty() {
        bail!("Username must not be empty");
    }
    let mut hash = Sha256::new();
    hash.update(homeserver.as_bytes());
    hash.update([0]);
    hash.update(username.as_bytes());
    Ok(format!("{:x}", hash.finalize()))
}

pub fn account_data_path(homeserver: &str, username: &str) -> Result<PathBuf> {
    let digest = account_id(homeserver, username)?;
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    Ok(home.join(".local/share/bloom-tui").join(digest))
}

fn prompt(label: &str) -> Result<String> {
    print!("{label}");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(input.trim().to_owned())
}

pub async fn authenticate() -> Result<Bloom> {
    let homeserver = match std::env::var("BLOOM_HOMESERVER") {
        Ok(value) => normalize_homeserver(&value)?,
        Err(_) => normalize_homeserver(&prompt("Homeserver URL: ")?)?,
    };
    let username = prompt("Username: ")?;
    if username.trim().is_empty() {
        bail!("Username must not be empty");
    }
    let data_dir = account_data_path(&homeserver, &username)?;
    let account = account_id(&homeserver, &username)?;
    let core = Bloom::open(&homeserver, &data_dir, &account, Arc::new(OsKeyring))
        .await
        .context(
            "Could not open Bloom account. Check OS keychain access; no plaintext fallback is used",
        )?;
    match core.restore().await {
        Ok(true) => Ok(core),
        Ok(false) => {
            let password = rpassword::prompt_password("Password: ")
                .context("Could not read password securely")?;
            let login_result = core.login(username.trim(), &password).await;
            drop(password);
            login_result
                .context("Login failed or credentials could not be saved to the OS keychain")?;
            Ok(core)
        }
        Err(error) => Err(error).context("Could not restore session from OS keychain"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_paths_are_stable_and_isolated() {
        let first = account_data_path("https://example.org/", "@alice:example.org").unwrap();
        assert_eq!(
            first,
            account_data_path("https://example.org", "@alice:example.org").unwrap()
        );
        assert_ne!(
            first,
            account_data_path("https://example.org", "@bob:example.org").unwrap()
        );
        assert_ne!(
            first,
            account_data_path("https://other.org", "@alice:example.org").unwrap()
        );
    }

    #[test]
    fn account_ids_and_paths_avoid_colon_namespace_collisions() {
        let first_server = "https://example.org/proxy";
        let first_user = "@alice:example.org";
        let second_server = "https://example.org/proxy:@alice";
        let second_user = "example.org";

        assert_ne!(
            account_id(first_server, first_user).unwrap(),
            account_id(second_server, second_user).unwrap()
        );
        assert_ne!(
            account_data_path(first_server, first_user).unwrap(),
            account_data_path(second_server, second_user).unwrap()
        );
    }

    #[test]
    fn account_ids_are_stable_for_canonical_homeserver_variants() {
        assert_eq!(
            account_id("https://EXAMPLE.org:443/proxy///", " @alice:example.org ").unwrap(),
            account_id("https://example.org/proxy", "@alice:example.org").unwrap()
        );
    }

    #[test]
    fn homeserver_validation_normalizes_and_rejects_empty_or_invalid() {
        assert_eq!(
            normalize_homeserver(" https://example.org/// ").unwrap(),
            "https://example.org"
        );
        assert!(normalize_homeserver(" ").is_err());
        assert!(normalize_homeserver("example.org").is_err());
    }

    #[test]
    fn homeserver_equivalents_have_identical_canonical_forms() {
        assert_eq!(
            normalize_homeserver("http://LOCALHOST:8008").unwrap(),
            normalize_homeserver("http://localhost:8008/").unwrap()
        );
        assert_eq!(
            normalize_homeserver("https://EXAMPLE.org:443/proxy///").unwrap(),
            "https://example.org/proxy"
        );
    }

    #[test]
    fn homeserver_rejects_unsafe_or_malformed_urls() {
        for input in [
            "ftp://example.org",
            "http:///missing-host",
            "http://user:secret@example.org",
            "http://user@example.org",
            "https://example.org/path?query=1",
            "https://example.org/path#fragment",
        ] {
            assert!(normalize_homeserver(input).is_err(), "accepted {input}");
        }
    }
}

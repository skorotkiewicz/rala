use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use argon2::password_hash::PasswordHash;
use russh::keys::ssh_key::PublicKey;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub title: String,
    pub domain: String,
    pub tls: bool,
    pub tls_certificate: Option<PathBuf>,
    pub tls_private_key: Option<PathBuf>,
    pub description: String,
    pub markdown_description: String,
    pub git_bind: std::net::SocketAddr,
    pub web_bind: std::net::SocketAddr,
    pub data_dir: PathBuf,
    pub clone_prefix: String,
    pub style: Option<PathBuf>,
    pub logo: Option<PathBuf>,
    pub favicon: Option<PathBuf>,
    pub accounts: Vec<Account>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Account {
    pub name: String,
    #[serde(default)]
    pub ssh_keys: Vec<String>,
    pub password_hash: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            title: "rala".into(),
            domain: "localhost".into(),
            tls: true,
            tls_certificate: None,
            tls_private_key: None,
            description: "Static sites published with Git".into(),
            markdown_description: String::new(),
            git_bind: "0.0.0.0:2222".parse().unwrap(),
            web_bind: "0.0.0.0:8080".parse().unwrap(),
            data_dir: "data".into(),
            clone_prefix: "https://localhost:8080 ssh://git@localhost:2222".into(),
            style: None,
            logo: None,
            favicon: None,
            accounts: vec![],
        }
    }
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let source =
            fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let mut config: Self = toml::from_str(&source).context("parse configuration")?;
        config.validate()?;
        let base = path.parent().unwrap_or(Path::new("."));
        for value in [&mut config.data_dir].into_iter().chain(
            [
                &mut config.tls_certificate,
                &mut config.tls_private_key,
                &mut config.style,
                &mut config.logo,
                &mut config.favicon,
            ]
            .into_iter()
            .filter_map(Option::as_mut),
        ) {
            if value.is_relative() {
                *value = base.join(&*value);
            }
        }
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(!self.domain.is_empty(), "domain must not be empty");
        ensure!(
            self.tls_certificate.is_some() == self.tls_private_key.is_some(),
            "tls_certificate and tls_private_key must be provided together"
        );
        let mut names = HashSet::new();
        let mut keys = HashSet::new();
        for account in &self.accounts {
            ensure!(
                valid_name(&account.name) && account.name != "git",
                "invalid or reserved account name: {}",
                account.name
            );
            ensure!(
                names.insert(&account.name),
                "duplicate account: {}",
                account.name
            );
            ensure!(
                !account.ssh_keys.is_empty() || account.password_hash.is_some(),
                "account {} needs an SSH key or password",
                account.name
            );
            for key in &account.ssh_keys {
                let key = PublicKey::from_openssh(key).context("invalid SSH public key")?;
                ensure!(
                    keys.insert(key.to_bytes()?),
                    "SSH key belongs to more than one account"
                );
            }
            if let Some(hash) = &account.password_hash {
                let parsed = PasswordHash::new(hash)
                    .map_err(|e| anyhow::anyhow!("invalid password hash: {e}"))?;
                if parsed.algorithm.as_str() != "argon2id" {
                    bail!("password hashes must use argon2id");
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn names_cannot_escape_storage() {
        for name in ["", "..", "../alice", "a/b", "a.git", "-x", "a\\b", "a b"] {
            assert!(!valid_name(name), "{name}");
        }
        assert!(valid_name("my-website_2"));
    }
    #[test]
    fn rejects_partial_tls_configuration() {
        let config = Config {
            tls_certificate: Some("cert.pem".into()),
            ..Config::default()
        };
        assert!(config.validate().is_err());
    }
}

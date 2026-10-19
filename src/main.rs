mod config;
mod ssh;
mod storage;
mod web;

use anyhow::{Context, Result, ensure};
use argon2::{Argon2, PasswordHasher, password_hash::SaltString};
use clap::{Parser, Subcommand};
use config::{Account, Config};
use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};
use storage::Storage;

#[derive(Parser)]
#[command(version, about = "Publish static sites with a Git push")]
struct Cli {
    #[arg(long, global = true, default_value = "config.toml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Option<Action>,
}

#[derive(Subcommand)]
enum Action {
    /// Run the SSH and web servers. This is the default command.
    Serve,
    /// Create an account in the configuration file.
    Create {
        #[command(subcommand)]
        resource: Create,
    },
}

#[derive(Subcommand)]
enum Create {
    /// Add an account. Without --ssh-key, prompt for a password.
    User {
        name: String,
        #[arg(long)]
        ssh_key: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rala=info".into()),
        )
        .init();
    let cli = Cli::parse();
    if let Some(Action::Create {
        resource: Create::User { name, ssh_key },
    }) = cli.command
    {
        return create_user(&cli.config, name, ssh_key);
    }
    let config = Config::load(&cli.config)?;
    let storage = Arc::new(Storage::new(config)?);
    let _ = rustls::crypto::ring::default_provider().install_default();
    tokio::select! {
        result = ssh::serve(storage.clone()) => result.context("SSH server"),
        result = web::serve(storage) => result.context("web server"),
        result = tokio::signal::ctrl_c() => { result?; tracing::info!("shutting down"); Ok(()) }
    }
}

fn create_user(path: &Path, name: String, ssh_key: Option<PathBuf>) -> Result<()> {
    ensure!(
        config::valid_name(&name) && name != "git",
        "invalid or reserved account name"
    );
    let mut doc: toml::Value = if path.exists() {
        toml::from_str(&std::fs::read_to_string(path)?)?
    } else {
        toml::Value::try_from(Config::default())?
    };
    let existing: Config = doc.clone().try_into()?;
    existing.validate()?;
    ensure!(
        !existing.accounts.iter().any(|a| a.name == name),
        "account {name} already exists"
    );
    let mut account = Account {
        name: name.clone(),
        ssh_keys: vec![],
        password_hash: None,
    };
    if let Some(key_path) = ssh_key {
        let key = std::fs::read_to_string(&key_path)
            .with_context(|| format!("read {}", key_path.display()))?;
        let parsed = russh::keys::ssh_key::PublicKey::from_openssh(key.trim())?;
        account.ssh_keys.push(parsed.to_openssh()?);
    } else {
        let password = rpassword::prompt_password("Password: ")?;
        ensure!(!password.is_empty(), "password must not be empty");
        let confirmation = rpassword::prompt_password("Confirm password: ")?;
        ensure!(password == confirmation, "passwords do not match");
        account.password_hash = Some(
            Argon2::default()
                .hash_password(
                    password.as_bytes(),
                    &SaltString::encode_b64(&rand::random::<[u8; 16]>())
                        .map_err(|e| anyhow::anyhow!("generate password salt: {e}"))?,
                )
                .map_err(|e| anyhow::anyhow!("hash password: {e}"))?
                .to_string(),
        );
    }
    let table = doc
        .as_table_mut()
        .context("configuration must be a TOML table")?;
    table
        .entry("accounts".to_owned())
        .or_insert_with(|| toml::Value::Array(vec![]))
        .as_array_mut()
        .context("accounts must be an array")?
        .push(toml::Value::try_from(account)?);
    let updated: Config = doc.clone().try_into()?;
    updated.validate()?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(toml::to_string_pretty(&doc)?.as_bytes())?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    println!(
        "Created account {name} in {}. Restart rala to load account changes.",
        path.display()
    );
    Ok(())
}

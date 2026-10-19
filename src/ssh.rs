use std::{collections::HashMap, net::SocketAddr, process::Stdio, sync::Arc, time::Duration};

use anyhow::{Context, Result, ensure};
use argon2::{Argon2, PasswordHash, PasswordVerifier};
use russh::{
    Channel, ChannelId,
    keys::{Algorithm, PrivateKey, PublicKey},
    server::{self, Auth, Handler, Msg, Server, Session},
};
use tokio::io::AsyncReadExt;

use crate::storage::{Storage, git, parse_repo};

pub async fn serve(storage: Arc<Storage>) -> Result<()> {
    let host_path = storage.config.data_dir.join("ssh_host_ed25519_key");
    let key = if host_path.exists() {
        russh::keys::load_secret_key(&host_path, None)?
    } else {
        let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)?;
        let mut file = tempfile::NamedTempFile::new_in(&storage.config.data_dir)?;
        use std::io::Write;
        file.write_all(
            key.to_openssh(russh::keys::ssh_key::LineEnding::LF)?
                .as_bytes(),
        )?;
        file.as_file().sync_all()?;
        file.persist_noclobber(&host_path)?;
        key
    };
    let config = Arc::new(server::Config {
        keys: vec![key],
        inactivity_timeout: Some(Duration::from_secs(300)),
        auth_rejection_time: Duration::from_secs(2),
        auth_rejection_time_initial: Some(Duration::ZERO),
        ..Default::default()
    });
    let address = storage.config.git_bind;
    let listener = tokio::net::TcpListener::bind(address).await?;
    tracing::info!(%address, "Git SSH listening");
    GitServer { storage }
        .run_on_socket(config, &listener)
        .await?;
    Ok(())
}

struct GitServer {
    storage: Arc<Storage>,
}

impl Server for GitServer {
    type Handler = Connection;
    fn new_client(&mut self, _: Option<SocketAddr>) -> Connection {
        Connection {
            storage: self.storage.clone(),
            account: None,
            channels: HashMap::new(),
        }
    }
    fn handle_session_error(&mut self, error: anyhow::Error) {
        tracing::debug!(%error, "SSH session ended");
    }
}

struct Connection {
    storage: Arc<Storage>,
    account: Option<String>,
    channels: HashMap<ChannelId, Channel<Msg>>,
}

fn reject() -> Auth {
    Auth::Reject {
        proceed_with_methods: None,
        partial_success: false,
    }
}

impl Connection {
    fn key_account(&self, user: &str, key: &PublicKey) -> Option<String> {
        self.storage
            .config
            .accounts
            .iter()
            .find(|a| {
                (user == "git" || user == a.name)
                    && a.ssh_keys.iter().any(|candidate| {
                        PublicKey::from_openssh(candidate)
                            .is_ok_and(|k| k.key_data() == key.key_data())
                    })
            })
            .map(|a| a.name.clone())
    }
}

impl Handler for Connection {
    type Error = anyhow::Error;

    async fn auth_publickey_offered(&mut self, user: &str, key: &PublicKey) -> Result<Auth> {
        Ok(if self.key_account(user, key).is_some() {
            Auth::Accept
        } else {
            reject()
        })
    }

    async fn auth_publickey(&mut self, user: &str, key: &PublicKey) -> Result<Auth> {
        self.account = self.key_account(user, key);
        Ok(if self.account.is_some() {
            Auth::Accept
        } else {
            reject()
        })
    }

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth> {
        let accounts = self.storage.config.accounts.clone();
        let user = user.to_owned();
        let password = password.to_owned();
        self.account = tokio::task::spawn_blocking(move || {
            let matches: Vec<_> = accounts
                .iter()
                .filter(|a| user == "git" || user == a.name)
                .filter(|a| {
                    a.password_hash.as_ref().is_some_and(|hash| {
                        PasswordHash::new(hash).is_ok_and(|h| {
                            Argon2::default()
                                .verify_password(password.as_bytes(), &h)
                                .is_ok()
                        })
                    })
                })
                .collect();
            if matches.len() == 1 {
                Some(matches[0].name.clone())
            } else {
                None
            }
        })
        .await?;
        Ok(if self.account.is_some() {
            Auth::Accept
        } else {
            reject()
        })
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: server::ChannelOpenHandle,
        _: &mut Session,
    ) -> Result<()> {
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn channel_close(&mut self, channel: ChannelId, _: &mut Session) -> Result<()> {
        self.channels.remove(&channel);
        Ok(())
    }

    async fn exec_request(
        &mut self,
        id: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<()> {
        let parsed = parse_command(data);
        let Some(channel) = self.channels.remove(&id) else {
            return Ok(());
        };
        let allowed = parsed.as_ref().is_ok_and(|(service, owner, _)| {
            self.account.is_some()
                && (service == "git-upload-pack" || self.account.as_deref() == Some(owner.as_str()))
        });
        if !allowed {
            session.channel_failure(id)?;
            session.extended_data(
                id,
                1,
                b"Repository access denied or invalid Git command.\n".to_vec(),
            )?;
            session.exit_status_request(id, 1)?;
            session.eof(id)?;
            session.close(id)?;
            return Ok(());
        }
        let (service, owner, repo) = parsed?;
        let storage = self.storage.clone();
        let handle = session.handle();
        session.channel_success(id)?;
        tokio::spawn(async move {
            let mut stream = channel.into_stream();
            let result = run_git(&mut stream, id, &handle, storage, &service, &owner, &repo).await;
            let code = match result {
                Ok(code) => code,
                Err(error) => {
                    tracing::warn!(error = %format!("{error:#}"), %owner, %repo, "Git request failed");
                    let _ = handle
                        .extended_data(id, 1, format!("rala: {error:#}\n").into_bytes())
                        .await;
                    1
                }
            };
            let _ = handle.exit_status_request(id, code).await;
            let _ = handle.eof(id).await;
            let _ = handle.close(id).await;
        });
        Ok(())
    }
}

fn parse_command(data: &[u8]) -> Result<(String, String, String)> {
    let command = std::str::from_utf8(data)?;
    let (service, path) = command.split_once(' ').context("expected Git command")?;
    ensure!(
        ["git-receive-pack", "git-upload-pack"].contains(&service),
        "unsupported command"
    );
    let path = path
        .strip_prefix('\'')
        .and_then(|p| p.strip_suffix('\''))
        .unwrap_or(path);
    let (owner, repo) = parse_repo(path)?;
    Ok((service.into(), owner.into(), repo.into()))
}

async fn run_git(
    stream: &mut russh::ChannelStream<Msg>,
    id: ChannelId,
    handle: &server::Handle,
    storage: Arc<Storage>,
    service: &str,
    owner: &str,
    repo: &str,
) -> Result<u32> {
    let lock = storage.lock(owner, repo);
    let _guard = lock.lock().await;
    let path = if service == "git-receive-pack" {
        storage.initialize(owner, repo).await?
    } else {
        let path = storage.repo(owner, repo);
        ensure!(path.is_dir(), "repository does not exist");
        path
    };
    tracing::debug!(%service, %owner, %repo, "starting Git request");
    let mut child = git()
        .arg(service.strip_prefix("git-").unwrap())
        .arg(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let (mut reader, mut writer) = tokio::io::split(stream);
    // A completed child may leave the client waiting for exit status, so input is cancellable.
    let input = async move {
        let count = tokio::io::copy(&mut reader, &mut stdin).await?;
        tracing::debug!(count, "Git input closed");
        drop(stdin);
        Ok::<_, std::io::Error>(())
    };
    tokio::pin!(input);
    let output = async { tokio::io::copy(&mut stdout, &mut writer).await };
    let errors = async {
        let mut buffer = [0; 8192];
        loop {
            let count = stderr.read(&mut buffer).await?;
            if count == 0 {
                break;
            }
            handle
                .extended_data(id, 1, buffer[..count].to_vec())
                .await
                .map_err(|_| std::io::Error::other("SSH channel closed"))?;
        }
        Ok::<_, std::io::Error>(())
    };
    let completion = async {
        let (status, output, errors) = tokio::join!(child.wait(), output, errors);
        output?;
        errors?;
        status
    };
    tokio::pin!(completion);
    let status = tokio::select! {
        status = &mut completion => status?,
        result = &mut input => { result?; completion.await? }
    };
    tracing::debug!(?status, %service, "Git process finished");
    if status.success() && service == "git-receive-pack" {
        storage.publish(owner, repo).await.context("publish site")?;
    }
    Ok(status.code().unwrap_or(1) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn command_is_never_a_shell() {
        assert!(parse_command(b"git-receive-pack '/alice/site.git'").is_ok());
        for command in [
            "sh -c id",
            "git-receive-pack 'alice/a.git';id",
            "git-upload-pack alice/../bob/a.git",
        ] {
            assert!(parse_command(command.as_bytes()).is_err());
        }
    }
}

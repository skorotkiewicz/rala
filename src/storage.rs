use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result, ensure};
use tokio::{process::Command, sync::Mutex as AsyncMutex};

use crate::config::{Config, valid_name};

pub struct Storage {
    pub config: Config,
    locks: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
}

impl Storage {
    pub fn new(config: Config) -> Result<Self> {
        std::fs::create_dir_all(config.data_dir.join("repos"))?;
        std::fs::create_dir_all(config.data_dir.join("sites"))?;
        Ok(Self {
            config,
            locks: Mutex::new(HashMap::new()),
        })
    }

    pub fn lock(&self, owner: &str, repo: &str) -> Arc<AsyncMutex<()>> {
        self.locks
            .lock()
            .unwrap()
            .entry(format!("{owner}/{repo}"))
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }

    pub fn repo(&self, owner: &str, repo: &str) -> PathBuf {
        self.config
            .data_dir
            .join("repos")
            .join(owner)
            .join(format!("{repo}.git"))
    }

    pub fn site(&self, owner: &str, repo: &str) -> PathBuf {
        self.config.data_dir.join("sites").join(owner).join(repo)
    }

    pub async fn initialize(&self, owner: &str, repo: &str) -> Result<PathBuf> {
        let path = self.repo(owner, repo);
        if !path.exists() {
            tokio::fs::create_dir_all(path.parent().unwrap()).await?;
            checked(
                git()
                    .args(["init", "--bare", "--initial-branch=main"])
                    .arg(&path),
            )
            .await?;
            checked(git().arg("--git-dir").arg(&path).args([
                "config",
                "receive.denyDeleteCurrent",
                "ignore",
            ]))
            .await?;
        }
        Ok(path)
    }

    // Each snapshot is immutable. Switching a small pointer file publishes it atomically.
    pub async fn publish(&self, owner: &str, repo: &str) -> Result<()> {
        let repository = self.repo(owner, repo);
        let root = self.site(owner, repo);
        tokio::fs::create_dir_all(&root).await?;
        let mut branch = None;
        for name in ["main", "master"] {
            let result = git()
                .arg("--git-dir")
                .arg(&repository)
                .args(["rev-parse", "--verify", &format!("refs/heads/{name}")])
                .output()
                .await?;
            if result.status.success() {
                branch = Some((name, String::from_utf8(result.stdout)?.trim().to_owned()));
                break;
            }
        }
        let Some((branch, revision)) = branch else {
            match tokio::fs::remove_file(root.join("current")).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            return Ok(());
        };
        let snapshot = root.join(&revision);
        if !snapshot.exists() {
            let stage = tempfile::Builder::new()
                .prefix(".stage-")
                .tempdir_in(&root)?;
            let archive = tempfile::tempfile()?;
            checked(
                git()
                    .arg("--git-dir")
                    .arg(&repository)
                    .args(["archive", "--format=tar", &revision])
                    .stdout(Stdio::from(archive.try_clone()?)),
            )
            .await?;
            let destination = stage.path().to_path_buf();
            tokio::task::spawn_blocking(move || extract(archive, &destination)).await??;
            tokio::fs::rename(stage.path(), &snapshot).await?;
        }
        let mut pointer = tempfile::NamedTempFile::new_in(&root)?;
        use std::io::Write;
        writeln!(pointer, "{revision}")?;
        pointer.as_file().sync_all()?;
        pointer.persist(root.join("current"))?;
        checked(git().arg("--git-dir").arg(&repository).args([
            "symbolic-ref",
            "HEAD",
            &format!("refs/heads/{branch}"),
        ]))
        .await?;
        Ok(())
    }

    pub async fn published_root(&self, owner: &str, repo: &str) -> Result<PathBuf> {
        let root = self.site(owner, repo);
        let revision = tokio::fs::read_to_string(root.join("current")).await?;
        let revision = revision.trim();
        ensure!(
            [40, 64].contains(&revision.len()) && revision.bytes().all(|c| c.is_ascii_hexdigit()),
            "invalid site pointer"
        );
        Ok(root.join(revision))
    }
}

pub fn git() -> Command {
    let mut cmd = Command::new("git");
    // Never inherit client hooks, alternates, config overrides, or a work tree.
    for (key, _) in std::env::vars_os() {
        if key.to_str().is_some_and(|s| s.starts_with("GIT_")) {
            cmd.env_remove(key);
        }
    }
    cmd.env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .kill_on_drop(true);
    cmd
}

pub async fn checked(command: &mut Command) -> Result<()> {
    let result = command
        .stderr(Stdio::piped())
        .spawn()
        .context("execute git, which must be installed")?
        .wait_with_output()
        .await?;
    ensure!(
        result.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    Ok(())
}

fn extract(mut archive: std::fs::File, destination: &Path) -> Result<()> {
    use std::io::{Seek, SeekFrom};
    archive.seek(SeekFrom::Start(0))?;
    for entry in tar::Archive::new(archive).entries()? {
        let mut entry = entry?;
        // Git emits the commit ID as a global PAX metadata record.
        if entry.header().entry_type().is_pax_global_extensions() {
            continue;
        }
        let path = entry.path()?;
        ensure!(
            path.components().all(
                |c| matches!(c, std::path::Component::Normal(s) if !s.eq_ignore_ascii_case(".git"))
            ),
            "unsafe published path"
        );
        ensure!(
            entry.header().entry_type().is_file() || entry.header().entry_type().is_dir(),
            "sites cannot contain symlinks, submodules, or special files"
        );
        ensure!(entry.unpack_in(destination)?, "path escapes site");
    }
    Ok(())
}

pub fn parse_repo(path: &str) -> Result<(&str, &str)> {
    let path = path.strip_prefix('/').unwrap_or(path);
    let (owner, repo) = path
        .split_once('/')
        .context("expected account/repository.git")?;
    let repo = repo
        .strip_suffix(".git")
        .context("repository must end in .git")?;
    ensure!(
        valid_name(owner) && valid_name(repo),
        "invalid repository path"
    );
    Ok((owner, repo))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn publishes_git_archive_and_keeps_previous_snapshot_on_failure() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::new(Config {
            data_dir: directory.path().join("data"),
            ..Default::default()
        })
        .unwrap();
        let repository = storage.initialize("alice", "site").await.unwrap();
        let work = directory.path().join("work");
        checked(git().args(["init", "--initial-branch=main"]).arg(&work))
            .await
            .unwrap();
        std::fs::write(work.join("index.html"), "hello").unwrap();
        checked(git().arg("-C").arg(&work).args(["add", "."]))
            .await
            .unwrap();
        checked(git().arg("-C").arg(&work).args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-m",
            "site",
        ]))
        .await
        .unwrap();
        checked(
            git()
                .arg("-C")
                .arg(&work)
                .arg("push")
                .arg(&repository)
                .arg("main"),
        )
        .await
        .unwrap();
        storage.publish("alice", "site").await.unwrap();
        let original = storage.published_root("alice", "site").await.unwrap();
        assert_eq!(
            std::fs::read_to_string(original.join("index.html")).unwrap(),
            "hello"
        );
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/passwd", work.join("secret")).unwrap();
            checked(git().arg("-C").arg(&work).args(["add", "."]))
                .await
                .unwrap();
            checked(git().arg("-C").arg(&work).args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-m",
                "link",
            ]))
            .await
            .unwrap();
            checked(
                git()
                    .arg("-C")
                    .arg(&work)
                    .arg("push")
                    .arg(&repository)
                    .arg("main"),
            )
            .await
            .unwrap();
            assert!(storage.publish("alice", "site").await.is_err());
            assert_eq!(
                storage.published_root("alice", "site").await.unwrap(),
                original
            );
            assert!(!original.join("secret").exists());
        }
    }

    #[test]
    fn repository_paths_are_strict() {
        assert_eq!(
            parse_repo("/alice/my-site.git").unwrap(),
            ("alice", "my-site")
        );
        for path in [
            "/alice/../../x.git",
            "//alice/x.git",
            "alice/x.git;id",
            "alice/x",
            "../x.git",
        ] {
            assert!(parse_repo(path).is_err(), "{path}");
        }
    }
}

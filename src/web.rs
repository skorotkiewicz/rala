use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
};

use anyhow::{Result, ensure};
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{Method, StatusCode, header},
    response::{Html, IntoResponse, Redirect, Response},
};
use percent_encoding::percent_decode_str;
use tokio::io::AsyncWriteExt;
use tower::ServiceExt;
use tower_http::services::ServeFile;

use crate::{
    config::valid_name,
    storage::{Storage, git},
};

pub fn router(storage: Arc<Storage>) -> Router {
    Router::new().fallback(handle).with_state(storage)
}

pub async fn serve(storage: Arc<Storage>) -> Result<()> {
    let address = storage.config.web_bind;
    let app = router(storage.clone());
    if storage.config.tls {
        let (certificate, key) = tls_files(&storage)?;
        let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(certificate, key).await?;
        tracing::info!(%address, "HTTPS listening");
        axum_server::bind_rustls(address, tls)
            .serve(app.into_make_service())
            .await?;
    } else {
        let listener = tokio::net::TcpListener::bind(address).await?;
        tracing::info!(%address, "HTTP listening");
        axum::serve(listener, app).await?;
    }
    Ok(())
}

fn tls_files(storage: &Storage) -> Result<(PathBuf, PathBuf)> {
    if let (Some(cert), Some(key)) = (
        &storage.config.tls_certificate,
        &storage.config.tls_private_key,
    ) {
        return Ok((cert.clone(), key.clone()));
    }
    let directory = storage.config.data_dir.join("tls");
    std::fs::create_dir_all(&directory)?;
    let certificate = directory.join("certificate.pem");
    let key = directory.join("private-key.pem");
    ensure!(
        certificate.exists() == key.exists(),
        "incomplete generated TLS pair in {}",
        directory.display()
    );
    if !certificate.exists() {
        let generated = rcgen::generate_simple_self_signed(vec![storage.config.domain.clone()])?;
        // NamedTempFile creates private keys with mode 0600 on Unix.
        use std::io::Write;
        let mut key_file = tempfile::NamedTempFile::new_in(&directory)?;
        key_file.write_all(generated.key_pair.serialize_pem().as_bytes())?;
        key_file.as_file().sync_all()?;
        key_file.persist_noclobber(&key)?;
        std::fs::write(&certificate, generated.cert.pem())?;
        tracing::info!(path = %directory.display(), "generated persistent self-signed TLS certificate");
    }
    Ok((certificate, key))
}

async fn handle(State(storage): State<Arc<Storage>>, request: Request) -> Response {
    let head = request.method() == Method::HEAD;
    let mut response = match dispatch(&storage, request).await {
        Ok(response) => response,
        Err(error) => {
            tracing::error!(%error, "web request failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "Request failed").into_response()
        }
    };
    response
        .headers_mut()
        .insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    if head {
        *response.body_mut() = Body::empty();
    }
    response
}

async fn dispatch(storage: &Storage, request: Request) -> Result<Response> {
    let Ok(path) = percent_decode_str(request.uri().path()).decode_utf8() else {
        return Ok(StatusCode::BAD_REQUEST.into_response());
    };
    if path.contains(['\\', '\0'])
        || path
            .split('/')
            .any(|s| s == "." || s == ".." || s.eq_ignore_ascii_case(".git"))
    {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    let path = path.into_owned();
    let segments: Vec<_> = path
        .trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    if segments.len() >= 2 && valid_name(segments[0]) && segments[1].ends_with(".git") {
        return smart_http(storage, &segments, request).await;
    }
    if request.method() != Method::GET && request.method() != Method::HEAD {
        return Ok((
            StatusCode::METHOD_NOT_ALLOWED,
            [(header::ALLOW, "GET, HEAD")],
        )
            .into_response());
    }
    if segments.is_empty() {
        return index(storage, None).await;
    }
    if segments.len() == 2 && segments[0] == "_rala" {
        let asset = match segments[1] {
            "style" => storage.config.style.as_ref(),
            "logo" => storage.config.logo.as_ref(),
            "favicon" => storage.config.favicon.as_ref(),
            _ => None,
        };
        return match asset {
            Some(file) => serve_file(file, request).await,
            None => Ok(StatusCode::NOT_FOUND.into_response()),
        };
    }
    let owner = segments[0];
    if !storage.config.accounts.iter().any(|a| a.name == owner) {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    if segments.len() == 1 {
        return index(storage, Some(owner)).await;
    }
    let repo = segments[1];
    if !valid_name(repo) {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    let Ok(root) = storage.published_root(owner, repo).await else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let relative: PathBuf = segments[2..].iter().collect();
    let file = root.join(relative);
    let Ok(metadata) = tokio::fs::metadata(&file).await else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let file = if metadata.is_dir() {
        if !path.ends_with('/') {
            let mut destination = format!("{}/", request.uri().path());
            if let Some(query) = request.uri().query() {
                destination.push('?');
                destination.push_str(query);
            }
            return Ok(Redirect::permanent(&destination).into_response());
        }
        file.join("index.html")
    } else {
        file
    };
    let Ok(canonical) = tokio::fs::canonicalize(&file).await else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let root = tokio::fs::canonicalize(root).await?;
    if !canonical.starts_with(root) {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    serve_file(&canonical, request).await
}

async fn serve_file(file: &Path, request: Request) -> Result<Response> {
    Ok(ServeFile::new(file)
        .oneshot(request)
        .await
        .unwrap()
        .into_response())
}

pub fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

async fn index(storage: &Storage, owner: Option<&str>) -> Result<Response> {
    let config = &storage.config;
    let mut html = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{}</title><meta name=\"description\" content=\"{}\"><style>body{{font:16px system-ui;max-width:70rem;margin:3rem auto;padding:0 1rem;line-height:1.6}}a{{color:#275cb5}}article{{border-top:1px solid #ccc;padding:1rem 0}}code{{overflow-wrap:anywhere}}img{{max-width:12rem}}</style>",
        escape(&config.title),
        escape(&config.description)
    );
    if config.style.is_some() {
        html.push_str("<link rel=\"stylesheet\" href=\"/_rala/style\">");
    }
    if config.favicon.is_some() {
        html.push_str("<link rel=\"icon\" href=\"/_rala/favicon\">");
    }
    html.push_str("</head><body><header>");
    if config.logo.is_some() {
        html.push_str("<img src=\"/_rala/logo\" alt=\"Server logo\">");
    }
    html.push_str(&format!(
        "<h1><a href=\"/\">{}</a></h1><p>{}</p>",
        escape(&config.title),
        escape(&config.description)
    ));
    let mut markdown = String::new();
    pulldown_cmark::html::push_html(
        &mut markdown,
        pulldown_cmark::Parser::new_ext(
            &config.markdown_description,
            pulldown_cmark::Options::ENABLE_TABLES,
        ),
    );
    html.push_str(&ammonia::clean(&markdown));
    html.push_str("</header><main>");
    let mut count = 0;
    let mut accounts: Vec<_> = config
        .accounts
        .iter()
        .filter(|a| owner.is_none_or(|owner| a.name == owner))
        .collect();
    accounts.sort_by_key(|a| &a.name);
    for account in accounts {
        let directory = config.data_dir.join("repos").join(&account.name);
        let mut repos = vec![];
        if let Ok(mut entries) = tokio::fs::read_dir(directory).await {
            while let Some(entry) = entries.next_entry().await? {
                let name = entry.file_name().to_string_lossy().into_owned();
                if let Some(repo) = name.strip_suffix(".git").filter(|r| valid_name(r)) {
                    repos.push(repo.to_owned());
                }
            }
        }
        repos.sort();
        for repo in repos {
            count += 1;
            let owner = &account.name;
            html.push_str(&format!(
                "<article><h2><a href=\"/{owner}/{repo}/\">{owner}/{repo}</a></h2>"
            ));
            for prefix in config.clone_prefix.split_whitespace() {
                html.push_str(&format!(
                    "<p><code>git clone {}/{owner}/{repo}.git</code></p>",
                    escape(prefix.trim_end_matches('/'))
                ));
            }
            html.push_str("</article>");
        }
    }
    if count == 0 {
        html.push_str("<p>No repositories yet. Push a site to publish it.</p>");
    }
    html.push_str("</main></body></html>");
    Ok(Html(html).into_response())
}

async fn smart_http(storage: &Storage, segments: &[&str], request: Request) -> Result<Response> {
    let owner = segments[0];
    let repo = segments[1].trim_end_matches(".git");
    if !valid_name(repo) || !storage.config.accounts.iter().any(|a| a.name == owner) {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    let path = storage.repo(owner, repo);
    if !path.is_dir() {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    let advertise = segments.len() == 4
        && segments[2..] == ["info", "refs"]
        && request.uri().query() == Some("service=git-upload-pack")
        && [Method::GET, Method::HEAD].contains(request.method());
    let upload =
        segments.len() == 3 && segments[2] == "git-upload-pack" && request.method() == Method::POST;
    if !advertise && !upload {
        return Ok(StatusCode::NOT_FOUND.into_response());
    }
    let mut command = git();
    command.args(["upload-pack", "--stateless-rpc"]);
    if advertise {
        command.arg("--advertise-refs");
    }
    command
        .arg(&path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = if upload {
        let body = match to_bytes(request.into_body(), 32 * 1024 * 1024).await {
            Ok(body) => body,
            Err(_) => return Ok(StatusCode::PAYLOAD_TOO_LARGE.into_response()),
        };
        let mut child = command.stdin(Stdio::piped()).spawn()?;
        let mut input = child.stdin.take().unwrap();
        // Write while draining stdout to avoid pipe deadlocks for large requests.
        let output = child.wait_with_output();
        let write = async move {
            input.write_all(&body).await?;
            input.shutdown().await
        };
        let (output, write) = tokio::join!(output, write);
        write?;
        output?
    } else {
        command.output().await?
    };
    ensure!(
        child.status.success(),
        "git upload-pack failed: {}",
        String::from_utf8_lossy(&child.stderr)
    );
    let content_type = if advertise {
        let mut header = b"001e# service=git-upload-pack\n0000".to_vec();
        header.extend_from_slice(&child.stdout);
        child.stdout = header;
        "application/x-git-upload-pack-advertisement"
    } else {
        "application/x-git-upload-pack-result"
    };
    Ok((
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        child.stdout,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tls_identity_persists() {
        let dir = tempfile::tempdir().unwrap();
        let config = crate::config::Config {
            data_dir: dir.path().into(),
            ..Default::default()
        };
        let storage = Storage::new(config).unwrap();
        let (cert, key) = tls_files(&storage).unwrap();
        let first = std::fs::read(&cert).unwrap();
        assert!(!std::fs::read(&key).unwrap().is_empty());
        assert_eq!(tls_files(&storage).unwrap(), (cert.clone(), key));
        assert_eq!(first, std::fs::read(cert).unwrap());
    }
}

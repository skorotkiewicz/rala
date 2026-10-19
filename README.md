# rala

Publish static websites with a Git push.

rala hosts your Git repositories and serves their committed files as websites.
Push to `main` to publish, with `master` as a fallback.

## Start

Building from source requires Rust, Git, and a native compiler toolchain.
From the repository root:

```sh
cargo build --release --locked
cp config.example.toml config.toml
./target/release/rala create user alice
./target/release/rala serve
```

Account creation prompts for a password. Add `--ssh-key ~/.ssh/id_ed25519.pub`
to use an SSH key instead. Restart rala after adding accounts or changing settings.

## Publish

From your website's Git repository, push the commit containing your built site
at the repository root:

```sh
git remote add rala ssh://alice@localhost:2222/alice/site.git
git push -u rala HEAD:main
```

The first push creates the repository. Visit
[localhost:8080/alice/site/](https://localhost:8080/alice/site/).
Sites and repositories are public.

## Configure

Edit `config.toml` to set your domain, ports, and appearance. See
[config.example.toml](config.example.toml) for available settings.

HTTPS is enabled by default with a persistent self-signed certificate, which
your browser must trust. For a public server, set `tls_certificate` and
`tls_private_key` to your trusted TLS certificate and key, or use `tls = false`
behind a TLS proxy.

See [GUIDE.md](GUIDE.md) for Arch Linux installation, systemd setup, TLS details,
and development instructions.

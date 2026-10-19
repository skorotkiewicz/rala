# rala

Host static websites by pushing to Git over SSH. The first push creates a bare
repository. rala publishes the committed files and lists the repository with its
clone commands on the server homepage.

Requires Rust with edition 2024 support, Git on `PATH`, and Unix for the integration
test. The server does not run build scripts. Commit the HTML, CSS, JavaScript, and
other files that you want to serve.

## Start a server

```sh
cargo build --release
cp target/release/rala ./rala
cp config.example.toml config.toml
./rala create user alice --config config.toml
./rala create user bob --ssh-key ~/.ssh/id_ed25519.pub --config config.toml
./rala --config config.toml
```

Without `--ssh-key`, account creation prompts for a password and confirmation. The
configuration stores an Argon2id hash. `--ssh-key` reads an OpenSSH public key file.
Each public key can belong to one account. Account names and repository names may
contain ASCII letters, digits, hyphens, and underscores and must start with a
letter or digit. The account name `git` is reserved for the shared SSH login.

`rala serve --config config.toml` also starts the server. Restart after editing
configuration or adding accounts. Account creation rewrites TOML formatting and
comments, preserves settings, and replaces the file atomically with private file
permissions. Keep your config backed up alongside the data directory.

## Publish a site

Run these commands inside your website repository:

```sh
git branch -M main
git remote add origin ssh://git@localhost:2222/alice/my-website.git
git add .
git commit -m "Publish website"
git push -u origin main
```

Open `https://localhost:8080/alice/my-website/`. Requests without the trailing slash
redirect so relative assets work. Directories serve `index.html`. File responses
support HEAD, ranges, and conditional requests. There is no SPA fallback.

The shared SSH login `git` identifies the account by its public key or password.
You can also log in as the account name, such as `ssh://alice@localhost:2222/...`.
Use the account name for password authentication if several accounts share the
same password. Only the owning account can push. Authenticated accounts can clone
any repository over SSH. HTTP and HTTPS cloning is public and read only:

```sh
git clone https://localhost:8080/alice/my-website.git
git clone ssh://git@localhost:2222/alice/my-website.git
```

The contents of `main` are published, with `master` as a fallback. Other branches
and tags stay in Git. Deleting both publishing branches removes the live site.
Updates use immutable snapshots and an atomic pointer replacement so a failed
deployment keeps the previous site. Symlinks and special files cannot be
published. A deployment failure returns an error, but Git refs may already have
advanced. Correct the content and push a new commit to publish it. Git metadata
is never served as site content. Submodules do not publish
their contents. Old snapshots remain under `data/sites`; there is no automatic
retention policy.

## Configuration and TLS

See [config.example.toml](config.example.toml) for all server settings. Relative
paths resolve against the configuration file's directory. `clone_prefix` is a
whitespace-separated list of URL prefixes used to show clone commands.
`description` is plain text. `markdown_description` renders sanitized Markdown on
the homepage. Optional `style`, `logo`, and `favicon` paths customize that page.

With `tls = true`, rala uses the configured certificate and key pair. If both are
omitted, it creates a self-signed certificate for `domain` under `data/tls/` and
reuses it on restart. Browsers require you to trust that certificate. For local
Git clients, trust it for a specific command:

```sh
git -c http.sslCAInfo=/absolute/path/to/data/tls/certificate.pem clone \
  https://localhost:8080/alice/my-website.git
```

For a public server, supply a trusted certificate chain and private key with
`tls_certificate` and `tls_private_key`. rala does not request or renew ACME
certificates. `domain` is the certificate hostname, without a scheme or port.
Set `tls = false` for plain HTTP, for example behind a TLS proxy, and update
`clone_prefix` to match your public URLs.

SSH host keys persist in `data/ssh_host_ed25519_key`. Generated private keys use
mode 0600. Back up this key to retain the server's SSH identity.

All repositories and websites are public. Uploaded HTML and scripts share the
server's origin, so use accounts for publishers you trust. There are no storage
quotas or build jobs. Git pushes run as the rala process user.

## Run with systemd

Install Git using your distribution's package manager, then install the binary,
configuration, and [service unit](packaging/rala.service):

```sh
cargo build --release
sudo useradd --system --user-group --home-dir /var/lib/rala \
  --no-create-home --shell /usr/sbin/nologin rala
sudo install -m 0755 target/release/rala /usr/local/bin/rala
sudo install -d -o rala -g rala -m 0750 /etc/rala
sudo install -o rala -g rala -m 0600 config.example.toml /etc/rala/config.toml
sudo install -m 0644 packaging/rala.service /etc/systemd/system/rala.service
```

Edit `/etc/rala/config.toml` and set the data directory to:

```toml
data_dir = "/var/lib/rala"
```

Set `domain` and `clone_prefix` to your server's public hostname and URLs. The
default ports are 2222 for SSH and 8080 for HTTPS. Put custom assets and supplied
TLS files under `/etc/rala/` and make them readable by `rala`. The service cannot
access home directories and can write only to `/var/lib/rala` and its private
temporary directories. systemd creates `/var/lib/rala` with private permissions.

Create accounts as the service user, then enable the service at boot and start it:

```sh
sudo -u rala /usr/local/bin/rala create user alice --config /etc/rala/config.toml
sudo systemctl daemon-reload
sudo systemctl enable --now rala.service
sudo systemctl status rala.service
```

For an SSH key account, use a public key file readable by `rala` with
`create user bob --ssh-key /path/to/bob.pub`. Run account creation as `rala` so
the rewritten configuration remains readable by the service. Restart after
configuration or account changes:

```sh
sudo systemctl restart rala.service
sudo journalctl -u rala.service -f
```

The unit restarts rala after failures and sends SIGINT on stop to use its existing
shutdown handler. Back up `/etc/rala/` and `/var/lib/rala/` to preserve accounts,
repositories, deployed sites, and server keys.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build
python3 tests/e2e.py
```

The integration test uses Python's standard library, Git, and OpenSSH. It creates
temporary accounts and checks authentication, publishing, cloning, isolation,
static file responses, branch selection, restart persistence, and TLS modes.
Set `RUST_LOG=rala=debug` to see server diagnostics.

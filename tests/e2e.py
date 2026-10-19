#!/usr/bin/env python3
"""Exercise the built binary with real Git, OpenSSH, HTTP, and TLS clients.

Run `cargo build` first, then `python3 tests/e2e.py` on Unix.
"""

import fcntl
import http.client
import os
from pathlib import Path
import pty
import select
import shlex
import shutil
import socket
import ssl
import subprocess
import tempfile
import termios
import time


BINARY = Path(__file__).resolve().parents[1] / "target/debug/rala"


def run(*args, cwd=None, env=None, succeeds=True):
    result = subprocess.run(args, cwd=cwd, env=env, text=True, capture_output=True, timeout=40)
    if succeeds:
        assert result.returncode == 0, f"{args}\n{result.stdout}\n{result.stderr}"
    else:
        assert result.returncode != 0, f"Unexpected success: {args}"
    return result


def port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def password_account(config):
    master, slave = pty.openpty()
    process = subprocess.Popen(
        [BINARY, "create", "user", "alice", "--config", config],
        stdin=slave, stdout=slave, stderr=slave, start_new_session=True,
        preexec_fn=lambda: fcntl.ioctl(0, termios.TIOCSCTTY, 0),
    )
    os.close(slave)
    transcript = b""
    try:
        for prompt in [b"Password:", b"Confirm password:"]:
            deadline = time.monotonic() + 15
            while prompt not in transcript:
                assert time.monotonic() < deadline, transcript.decode(errors="replace")
                ready, _, _ = select.select([master], [], [], 0.1)
                if ready:
                    try:
                        transcript += os.read(master, 4096)
                    except OSError as error:
                        raise AssertionError(transcript.decode(errors="replace")) from error
            os.write(master, b"test-password-123\n")
        assert process.wait(timeout=15) == 0, transcript
    finally:
        os.close(master)
        if process.poll() is None:
            process.kill()
            process.wait()


def exercise(root):
    ssh_port, web_port = port(), port()
    while ssh_port == web_port:
        web_port = port()
    config = root / "config.toml"
    config.write_text(f'''title = "<rala test>"
domain = "localhost"
tls = true
git_bind = "127.0.0.1:{ssh_port}"
web_bind = "127.0.0.1:{web_port}"
data_dir = "data"
description = "Test sites"
markdown_description = '**Hello** <script>alert(1)</script>'
clone_prefix = "https://localhost:{web_port} ssh://git@localhost:{ssh_port}"
style = "theme.css"
logo = "logo.svg"
favicon = "favicon.svg"
''')
    (root / "theme.css").write_text("body { color: navy; }")
    for name in ["logo.svg", "favicon.svg"]:
        (root / name).write_text('<svg xmlns="http://www.w3.org/2000/svg"></svg>')
    password_account(config)
    assert "$argon2id$" in config.read_text()
    assert "test-password-123" not in config.read_text()
    key = root / "bob-key"
    run("ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(key))
    run(str(BINARY), "create", "user", "bob", "--ssh-key", str(key) + ".pub", "--config", str(config))
    generated_config = root / "generated/config.toml"
    run(str(BINARY), "create", "user", "bob", "--ssh-key", str(key) + ".pub", "--config", str(generated_config))
    assert 'name = "bob"' in generated_config.read_text()
    assert generated_config.stat().st_mode & 0o077 == 0
    before = config.read_bytes()
    run(str(BINARY), "create", "user", "bob", "--ssh-key", str(key) + ".pub", "--config", str(config), succeeds=False)
    run(str(BINARY), "create", "user", "../escape", "--ssh-key", str(key) + ".pub", "--config", str(config), succeeds=False)
    run(str(BINARY), "create", "user", "duplicate-key", "--ssh-key", str(key) + ".pub", "--config", str(config), succeeds=False)
    assert config.read_bytes() == before
    askpass = root / "askpass"
    askpass.write_text("#!/bin/sh\nprintf '%s\\n' test-password-123\n")
    askpass.chmod(0o700)
    base_env = dict(os.environ, GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL="/dev/null", GIT_TERMINAL_PROMPT="0")
    ssh_base = ["ssh", "-o", "StrictHostKeyChecking=no", "-o", f"UserKnownHostsFile={root / 'known_hosts'}", "-o", "ConnectTimeout=5"]
    bob = dict(base_env, GIT_SSH_COMMAND=shlex.join(ssh_base + ["-i", str(key), "-o", "IdentitiesOnly=yes", "-o", "BatchMode=yes"]))
    alice = dict(base_env, GIT_SSH_COMMAND=shlex.join(ssh_base + ["-o", "PreferredAuthentications=password", "-o", "PubkeyAuthentication=no", "-o", "NumberOfPasswordPrompts=1"]), SSH_ASKPASS=str(askpass), SSH_ASKPASS_REQUIRE="force", DISPLAY=":0")
    log = open(root / "server.log", "w+")
    server = None
    use_tls = True

    def request(path="/", method="GET", headers=None):
        if use_tls:
            context = ssl.create_default_context(cafile=str(root / "data/tls/certificate.pem"))
            connection = http.client.HTTPSConnection("localhost", web_port, context=context, timeout=5)
        else:
            connection = http.client.HTTPConnection("localhost", web_port, timeout=5)
        connection.request(method, path, headers=headers or {})
        response = connection.getresponse()
        result = response.status, dict(response.getheaders()), response.read()
        connection.close()
        return result

    def start():
        nonlocal server
        server = subprocess.Popen([BINARY, "--config", config], cwd="/tmp", stdout=log, stderr=log, env=dict(os.environ, RUST_LOG="rala=debug"))
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            assert server.poll() is None, "server exited"
            try:
                if request()[0] == 200:
                    # The web listener may start before SSH.
                    with socket.create_connection(("127.0.0.1", ssh_port), timeout=1):
                        return
            except (OSError, ssl.SSLError):
                pass
            time.sleep(0.1)
        raise AssertionError("server did not become ready")

    def stop():
        if server and server.poll() is None:
            server.terminate()
            server.wait(timeout=10)

    try:
        start()
        # No interactive shell or command execution is available.
        denied = run(*ssh_base, "-i", str(key), "-o", "IdentitiesOnly=yes", "-o", "BatchMode=yes", "-p", str(ssh_port), "git@localhost", "touch /tmp/rala-must-not-exist", env=base_env, succeeds=False)
        assert "failed" in denied.stderr.lower() or "denied" in denied.stderr.lower()
        wrong_askpass = root / "wrong-askpass"
        wrong_askpass.write_text("#!/bin/sh\nprintf wrong-password\n")
        wrong_askpass.chmod(0o700)
        denied_password = dict(alice, SSH_ASKPASS=str(wrong_askpass))
        run("git", "ls-remote", f"ssh://git@localhost:{ssh_port}/alice/site.git", env=denied_password, succeeds=False)
        index = request()[2]
        assert b"&lt;rala test&gt;" in index and b"<strong>Hello</strong>" in index
        assert b"<script>" not in index
        assert request("/_rala/style")[2] == b"body { color: navy; }"
        assert request("/_rala/logo")[0] == request("/_rala/favicon")[0] == 200
        assert request("/", "HEAD")[2] == b""
        assert request("/", "POST")[0] == 405

        work = root / "work"
        run("git", "init", "--initial-branch=main", str(work), env=base_env)
        run("git", "config", "user.name", "Test", cwd=work, env=base_env)
        run("git", "config", "user.email", "test@example.com", cwd=work, env=base_env)
        (work / "index.html").write_text("<h1>version one</h1>")
        (work / "nested").mkdir()
        (work / "nested/index.html").write_text("nested")
        (work / "file.txt").write_text("abcdef")
        payload = os.urandom(3 * 1024 * 1024)
        (work / "large.bin").write_bytes(payload)
        run("git", "add", ".", cwd=work, env=base_env)
        run("git", "commit", "-m", "first site", cwd=work, env=base_env)
        remote = f"ssh://git@localhost:{ssh_port}/alice/site.git"
        run("git", "remote", "add", "origin", remote, cwd=work, env=base_env)
        run("git", "push", "origin", "main", cwd=work, env=alice)
        for _ in range(50):
            first = request("/alice/site/")
            if first[2] == b"<h1>version one</h1>":
                break
            time.sleep(0.1)
        assert first[2] == b"<h1>version one</h1>", first
        assert request("/alice/site?x=1")[1]["location"] == "/alice/site/?x=1"
        assert request("/alice/site/nested")[0] == 308
        assert request("/alice/site/nested/")[2] == b"nested"
        assert request("/alice/site/file.txt", headers={"Range": "bytes=1-3"})[2] == b"bcd"
        assert request("/alice/site/file.txt", "HEAD")[2] == b""
        for path in ["/alice/site/.git/config", "/alice/site/%2e%2e/current", "/alice/site/%2e%2e/%2e%2e/config.toml", "/alice/site/%5c..%5cconfig.toml", "/alice/site/%00", "/alice/site/missing"]:
            assert request(path)[0] == 404, path
        assert b"alice/site.git" in request()[2]
        assert request("/alice")[0] == 200
        run("git", "clone", remote, str(root / "ssh-clone"), env=alice)
        assert (root / "ssh-clone/index.html").read_text() == "<h1>version one</h1>"
        assert (root / "ssh-clone/large.bin").read_bytes() == payload
        http_env = dict(base_env, GIT_SSL_CAINFO=str(root / "data/tls/certificate.pem"))
        run("git", "clone", f"https://localhost:{web_port}/alice/site.git", str(root / "http-clone"), env=http_env)
        assert (root / "http-clone/file.txt").read_text() == "abcdef"
        assert (root / "http-clone/large.bin").read_bytes() == payload

        (work / "index.html").write_text("<h1>version two</h1>")
        (work / "file.txt").unlink()
        run("git", "add", "-A", cwd=work, env=base_env)
        run("git", "commit", "-m", "update", cwd=work, env=base_env)
        run("git", "push", "origin", "main", cwd=work, env=alice)
        assert request("/alice/site/")[2] == b"<h1>version two</h1>"
        assert request("/alice/site/file.txt")[0] == 404
        run("git", "push", "origin", "main", cwd=work, env=bob, succeeds=False)
        run("git", "push", f"ssh://git@localhost:{ssh_port}/bob/site.git", "main", cwd=work, env=bob)
        assert request("/bob/site/")[2] == b"<h1>version two</h1>"
        # Symlinks never expose files outside the published tree.
        (work / "secret").symlink_to(root / "config.toml")
        run("git", "add", "secret", cwd=work, env=base_env)
        run("git", "commit", "-m", "unsupported symlink", cwd=work, env=base_env)
        result = run("git", "push", "origin", "main", cwd=work, env=alice, succeeds=False)
        assert "publish site" in result.stderr
        assert request("/alice/site/secret")[0] == 404
        assert request("/alice/site/")[2] == b"<h1>version two</h1>"

        # Restore a valid main, then check master fallback and branch deletion.
        run("git", "rm", "secret", cwd=work, env=base_env)
        run("git", "commit", "-m", "valid site", cwd=work, env=base_env)
        run("git", "push", "origin", "main:master", cwd=work, env=alice, succeeds=False)
        run("git", "push", "origin", "main", cwd=work, env=alice)
        run("git", "-C", str(root / "http-clone"), "fetch", "origin", env=http_env)
        (work / "index.html").write_text("master branch")
        run("git", "add", ".", cwd=work, env=base_env)
        run("git", "commit", "-m", "master site", cwd=work, env=base_env)
        run("git", "push", "origin", "main:master", cwd=work, env=alice)
        assert request("/alice/site/")[2] == b"<h1>version two</h1>"
        run("git", "push", "origin", ":main", cwd=work, env=alice)
        assert request("/alice/site/")[2] == b"master branch"
        run("git", "push", "origin", ":master", cwd=work, env=alice)
        assert request("/alice/site/")[0] == 404

        cert = (root / "data/tls/certificate.pem").read_bytes()
        host = (root / "data/ssh_host_ed25519_key").read_bytes()
        assert (root / "data/ssh_host_ed25519_key").stat().st_mode & 0o077 == 0
        assert (root / "data/tls/private-key.pem").stat().st_mode & 0o077 == 0
        stop()
        start()
        assert (root / "data/tls/certificate.pem").read_bytes() == cert
        assert (root / "data/ssh_host_ed25519_key").read_bytes() == host
        assert request("/bob/site/")[0] == 200
        stop()
        shutil.copy(root / "data/tls/certificate.pem", root / "custom-cert.pem")
        shutil.copy(root / "data/tls/private-key.pem", root / "custom-key.pem")
        config.write_text('tls_certificate = "custom-cert.pem"\ntls_private_key = "custom-key.pem"\n' + config.read_text())
        start()
        assert request("/bob/site/")[0] == 200
        stop()
        config.write_text(config.read_text().replace("tls = true", "tls = false"))
        use_tls = False
        start()
        assert request("/bob/site/")[0] == 200
        print("PASS: CLI, password/key SSH auth, push/update, SSH/HTTP clone/fetch, account isolation, static paths, symlink rejection, branch fallback/deletion, TLS persistence/custom certs, HTTP mode")
    except Exception:
        log.flush()
        log.seek(0)
        print(log.read())
        raise
    finally:
        stop()
        log.close()


if __name__ == "__main__":
    with tempfile.TemporaryDirectory(prefix="rala-e2e-") as directory:
        exercise(Path(directory))

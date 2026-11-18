#!/usr/bin/env python3
"""Headless Google account login for the Google Drive upload module.

This uses Google Cloud SDK's own OAuth client.  The user does not need to
create an OAuth client ID: they open the URL printed by gcloud, sign in with
their Google account, and paste the verification code back into the dashboard.

The process speaks the same LOGIN_STATE line protocol as the Rust modules:

    LOGIN_STATE:device:<url>
    LOGIN_STATE:already:<email>
    LOGIN_STATE:done:<email>
    LOGIN_STATE:error:<message>
    LOGIN_STATE:cancelled
"""

from __future__ import annotations

import os
import pty
import re
import select
import shutil
import subprocess
import sys
import time
from urllib.parse import urlparse

STATE_PREFIX = "LOGIN_STATE:"
ANSI_RE = re.compile(r"\x1b\[[0-9;?]*[A-Za-z]")
URL_RE = re.compile(r"https://accounts\.google\.com/[^\s]+")


def state(value: str) -> None:
    print(STATE_PREFIX + value, flush=True)


def clean(text: str) -> str:
    return ANSI_RE.sub("", text).replace("\r", "")


def tail(text: str, limit: int = 600) -> str:
    text = " ".join(clean(text).split())
    return text[-limit:] if len(text) > limit else text


def gcloud_bin() -> str:
    bundled = "/opt/google-cloud-sdk/bin/gcloud"
    if os.path.isfile(bundled) and os.access(bundled, os.X_OK):
        return bundled
    found = shutil.which("gcloud")
    if found:
        return found
    state("error:gcloud is not installed in this image")
    raise SystemExit(1)


def gcloud_env() -> dict[str, str]:
    root = os.environ.get("PP_EXE_DIR") or os.path.dirname(os.path.abspath(__file__))
    config_dir = os.path.join(root, "config", "upload_gdrive", "gcloud")
    os.makedirs(config_dir, exist_ok=True)
    env = os.environ.copy()
    env["CLOUDSDK_CONFIG"] = config_dir
    env["PYTHONUNBUFFERED"] = "1"
    env["TERM"] = "dumb"

    # gcloud does not read the app's proxy settings automatically.  Without
    # this it hangs trying to reach accounts.google.com directly on a VPS
    # whose Google traffic must go through WARP.
    proxy = os.environ.get("PP_GCLOUD_PROXY", "").strip()
    if proxy:
        try:
            parsed = urlparse(proxy)
            scheme = (parsed.scheme or "http").lower()
            proxy_type = (
                "socks5"
                if scheme.startswith("socks")
                else "https"
                if scheme == "https"
                else "http"
            )
            if parsed.hostname and parsed.port:
                env["CLOUDSDK_PROXY_TYPE"] = proxy_type
                env["CLOUDSDK_PROXY_ADDRESS"] = parsed.hostname
                env["CLOUDSDK_PROXY_PORT"] = str(parsed.port)
                env["ALL_PROXY"] = proxy
                if proxy_type == "http":
                    env["HTTP_PROXY"] = proxy
                    env["HTTPS_PROXY"] = proxy
                elif proxy_type == "https":
                    env["HTTPS_PROXY"] = proxy
        except ValueError:
            pass
    return env


def run_gcloud(args: list[str]) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [gcloud_bin(), *args],
        env=gcloud_env(),
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=90,
    )


def active_account() -> str | None:
    try:
        result = run_gcloud(
            ["auth", "list", "--filter=status:ACTIVE", "--format=value(account)"]
        )
    except Exception:
        return None
    if result.returncode != 0:
        return None
    for line in result.stdout.splitlines():
        line = line.strip()
        if line:
            return line
    return None


def has_access_token() -> bool:
    try:
        result = run_gcloud(["auth", "print-access-token"])
    except Exception:
        return False
    return result.returncode == 0 and bool(result.stdout.strip())


def force_login() -> bool:
    value = os.environ.get("PP_FORCE_LOGIN", "").strip().lower()
    return value in {"1", "true", "yes"} or "--fresh" in sys.argv or "--force" in sys.argv


def read_until_url(master: int, proc: subprocess.Popen[bytes]) -> tuple[str | None, bytes]:
    output = b""
    deadline = time.time() + 120
    while time.time() < deadline:
        ready, _, _ = select.select([master], [], [], 0.5)
        if ready:
            try:
                chunk = os.read(master, 4096)
            except OSError:
                break
            if not chunk:
                break
            output += chunk
            match = URL_RE.search(clean(output.decode("utf-8", "replace")))
            if match:
                return match.group(0).rstrip(".,)"), output
        if proc.poll() is not None:
            break
    return None, output


def read_remaining(master: int, proc: subprocess.Popen[bytes]) -> bytes:
    output = b""
    deadline = time.time() + 120
    while time.time() < deadline:
        ready, _, _ = select.select([master], [], [], 0.5)
        if ready:
            try:
                chunk = os.read(master, 4096)
            except OSError:
                break
            if not chunk:
                break
            output += chunk
        if proc.poll() is not None:
            try:
                while True:
                    chunk = os.read(master, 4096)
                    if not chunk:
                        break
                    output += chunk
            except OSError:
                pass
            break
    return output


def main() -> int:
    if not force_login():
        account = active_account()
        if account and has_access_token():
            state("already:" + account)
            return 0

    try:
        master, slave = pty.openpty()
    except OSError as exc:
        state("error:could not create a pseudo-terminal: " + str(exc))
        return 1

    command = [
        gcloud_bin(),
        "auth",
        "login",
        "--force",
        "--no-launch-browser",
        "--enable-gdrive-access",
    ]
    try:
        proc = subprocess.Popen(
            command,
            stdin=slave,
            stdout=slave,
            stderr=slave,
            env=gcloud_env(),
            close_fds=True,
            preexec_fn=os.setsid,
        )
    except OSError as exc:
        os.close(slave)
        state("error:could not start gcloud: " + str(exc))
        return 1
    finally:
        try:
            os.close(slave)
        except OSError:
            pass

    url, initial_output = read_until_url(master, proc)
    if not url:
        detail = tail(initial_output.decode("utf-8", "replace"))
        try:
            proc.kill()
        except Exception:
            pass
        state("error:Google login did not return a URL" + (": " + detail if detail else ""))
        return 1

    state("device:" + url)

    code: str | None = None
    for line in sys.stdin:
        line = line.strip()
        if line in {"quit", "cancel"}:
            try:
                proc.kill()
            except Exception:
                pass
            state("cancelled")
            return 0
        if line.startswith("code:"):
            code = line[len("code:") :].strip()
            break

    if not code:
        try:
            proc.kill()
        except Exception:
            pass
        state("error:verification code was not provided")
        return 1

    try:
        os.write(master, (code + "\n").encode("utf-8"))
    except OSError as exc:
        state("error:could not submit verification code: " + str(exc))
        return 1

    final_output = read_remaining(master, proc)
    try:
        proc.wait(timeout=15)
    except Exception:
        try:
            proc.kill()
        except Exception:
            pass
        try:
            proc.wait(timeout=5)
        except Exception:
            pass

    if proc.returncode != 0:
        detail = tail(final_output.decode("utf-8", "replace"))
        # Do not echo the one-time verification code into the dashboard logs.
        if code:
            detail = detail.replace(code, "[code]")
        state("error:Google login failed" + (": " + detail if detail else ""))
        return 1

    account = active_account()
    if not account or not has_access_token():
        detail = tail(final_output.decode("utf-8", "replace"))
        if code:
            detail = detail.replace(code, "[code]")
        state("error:Google login finished but no access token is available" + (": " + detail if detail else ""))
        return 1

    state("done:" + account)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except KeyboardInterrupt:
        state("cancelled")
        raise SystemExit(0)

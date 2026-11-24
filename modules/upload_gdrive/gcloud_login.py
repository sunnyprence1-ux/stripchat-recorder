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
import re
import select
import shutil
import subprocess
import sys
import time
from urllib.parse import urlparse

STATE_PREFIX = "LOGIN_STATE:"
ANSI_RE = re.compile(r"\x1b\[[0-9;?]*[A-Za-z]")
URL_RE = re.compile(r"https://accounts\.google\.com/[^\s]+(?=\s)")
URL_TIMEOUT = 30
EXCHANGE_TIMEOUT = 45
ACCOUNT_TIMEOUT = 8
TOKEN_TIMEOUT = 12


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
    config_dir = os.environ.get("CLOUDSDK_CONFIG")
    if not config_dir:
        root = os.environ.get("PP_EXE_DIR") or os.getcwd()
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


def run_gcloud(args: list[str], timeout: int) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [gcloud_bin(), *args],
        env=gcloud_env(),
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=timeout,
    )


def active_account() -> str | None:
    try:
        result = run_gcloud(
            ["auth", "list", "--filter=status:ACTIVE", "--format=value(account)"],
            ACCOUNT_TIMEOUT,
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
        result = run_gcloud(["auth", "print-access-token"], TOKEN_TIMEOUT)
    except Exception:
        return False
    return result.returncode == 0 and bool(result.stdout.strip())


def force_login() -> bool:
    value = os.environ.get("PP_FORCE_LOGIN", "").strip().lower()
    return value in {"1", "true", "yes"} or "--fresh" in sys.argv or "--force" in sys.argv


def read_until_url(master: int, proc: subprocess.Popen[bytes]) -> tuple[str | None, bytes]:
    output = b""
    deadline = time.monotonic() + URL_TIMEOUT
    while time.monotonic() < deadline:
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
    deadline = time.monotonic() + EXCHANGE_TIMEOUT
    while time.monotonic() < deadline:
        ready, _, _ = select.select([master], [], [], 0.5)
        if ready:
            try:
                chunk = os.read(master, 4096)
            except OSError:
                break
            if not chunk:
                break
            output += chunk
        if not ready and proc.poll() is not None:
            break
    return output


def main() -> int:
    try:
        import pty
    except ImportError:
        state("error:Google Drive sign-in requires a Unix host or the Docker edition")
        return 1

    if not force_login():
        account = active_account()
        if account and has_access_token():
            state("already:" + account)
            return 0

    binary = gcloud_bin()
    try:
        env = gcloud_env()
        master, slave = pty.openpty()
    except OSError as exc:
        state("error:could not prepare Google sign-in: " + str(exc))
        return 1

    proc = None
    try:
        try:
            proc = subprocess.Popen(
                [binary, "auth", "login", "--force", "--no-launch-browser", "--enable-gdrive-access"],
                stdin=slave, stdout=slave, stderr=slave, env=env,
                close_fds=True, start_new_session=True,
            )
        finally:
            os.close(slave)

        url, initial_output = read_until_url(master, proc)
        if not url:
            if proc.poll() is None:
                state("error:Google sign-in did not produce a link within "
                      f"{URL_TIMEOUT} seconds. Check the recorder's connection to Google and its API proxy settings.")
            else:
                detail = tail(re.sub(r"https://\S+", "[URL]", initial_output.decode("utf-8", "replace")))
                state("error:Google sign-in could not start" + (": " + detail if detail else ""))
            return 1
        state("device:" + url)

        code = None
        for line in sys.stdin:
            line = line.strip()
            if line in {"quit", "cancel"}:
                state("cancelled")
                return 0
            if line.startswith("code:"):
                code = line[len("code:"):].strip()
                break

        if not code:
            state("error:verification code was not provided")
            return 1
        os.write(master, (code + "\n").encode("utf-8"))
        final_output = read_remaining(master, proc)
        try:
            returncode = proc.wait(timeout=1)
        except subprocess.TimeoutExpired:
            state("error:Google did not finish verifying the code within "
                  f"{EXCHANGE_TIMEOUT} seconds. Check the recorder's connection to Google and try again.")
            return 1
        if returncode != 0:
            detail = tail(final_output.decode("utf-8", "replace").replace(code, "[code]"))
            # Exclude OAuth URLs and any verification code echoed by the terminal.
            detail = re.sub(r"https://\S+", "[URL]", detail)
            state("error:Google sign-in failed" + (": " + detail if detail else ""))
            return 1

        account = active_account()
        if not account:
            state("error:Google sign-in finished but no active account was found")
            return 1
        state("done:" + account)
        return 0
    except OSError as exc:
        state("error:could not complete Google sign-in: " + str(exc))
        return 1
    finally:
        os.close(master)
        if proc is not None and proc.poll() is None:
            proc.kill()
            proc.wait()


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except KeyboardInterrupt:
        state("cancelled")
        raise SystemExit(0)

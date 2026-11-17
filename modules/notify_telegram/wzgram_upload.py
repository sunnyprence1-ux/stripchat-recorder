#!/usr/bin/env python3
"""WZGram file-part uploader used by notify_telegram.

The Rust module keeps ownership of login, topics, covers, caption, send and
cleanup. This sidecar only uploads one file's 512 KiB parts through WZGram and
prints the resulting InputFile/InputFileBig identity back to Rust.

It reuses the auth key from the existing grammers SQLite session, so no second
login or session migration is needed.
"""

import asyncio
import ipaddress
import os
import sqlite3
import struct
import sys
import time
import zlib
from urllib.parse import urlsplit

# WZGram reads several limits while its modules are imported, so set the
# resource caps before importing it.
os.environ.setdefault("WZGRAM_MAX_READ_AHEAD", "32")
os.environ.setdefault("WZGRAM_HANDLER_WORKERS", "2")
os.environ.setdefault("WZGRAM_MAX_INFLIGHT_MEDIA", "6")
os.environ.setdefault("WZGRAM_MAX_INFLIGHT_PACKETS", "16")
os.environ.setdefault("WZGRAM_MAX_PENDING_UPDATES", "64")
os.environ.setdefault("WZGRAM_MAX_LISTENERS", "64")

_ALPHABET = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"
_ENCODE = {c: i for i, c in enumerate(_ALPHABET)}


def _encode(raw: bytes) -> str:
    """WZGram/Pyrogram session-string base64 variant."""
    result = []
    for i in range(0, len(raw), 3):
        chunk = raw[i:i + 3]
        if len(chunk) == 3:
            n = (chunk[0] << 16) | (chunk[1] << 8) | chunk[2]
            result.append(_ALPHABET[(n >> 18) & 63])
            result.append(_ALPHABET[(n >> 12) & 63])
            result.append(_ALPHABET[(n >> 6) & 63])
            result.append(_ALPHABET[n & 63])
        elif len(chunk) == 2:
            n = (chunk[0] << 8) | chunk[1]
            result.append(_ALPHABET[(n >> 10) & 63])
            result.append(_ALPHABET[(n >> 4) & 63])
            result.append(_ALPHABET[(n << 2) & 63])
        else:
            n = chunk[0]
            result.append(_ALPHABET[(n >> 2) & 63])
            result.append(_ALPHABET[(n << 4) & 63])
    return "".join(result)


# Fallbacks match grammers' built-in DC options. Used only when a session row
# has no usable address, which should never happen for a normal login.
_KNOWN_DC_IPS = {
    1: "149.154.175.53",
    2: "149.154.167.41",
    3: "149.154.175.100",
    4: "149.154.167.92",
    5: "91.108.56.100",
}


def _split_dc_address(address: str) -> tuple[str, int]:
    """Split a grammers `dc_option.ipv4` value into (bare host, port).

    grammers stores `SocketAddrV4::to_string()`, e.g. "149.154.167.41:443".
    Pyrogram's session string instead takes the host and port as separate
    fields. Passing the whole socket address as the host makes connect() fail
    and would silently send every upload back through the Rust fallback.
    """
    value = (address or "").strip()
    if not value:
        return "", 443

    # Already a bare IP address.
    try:
        return str(ipaddress.ip_address(value)), 443
    except ValueError:
        pass

    # grammers IPv4: "1.2.3.4:443"; IPv6: "[2001:db8::1]:443".
    host, sep, port_text = value.rpartition(":")
    if sep and host:
        candidate = host[1:-1] if host.startswith("[") and host.endswith("]") else host
        try:
            port = int(port_text)
            if not 1 <= port <= 65535:
                raise ValueError("port out of range")
            return str(ipaddress.ip_address(candidate)), port
        except ValueError:
            pass

    # Last resort: urlsplit understands bracketed IPv6 addresses.
    try:
        host = urlsplit("//" + value).hostname or ""
        if host:
            return host, 443
    except ValueError:
        pass
    return value, 443


def _session_string(
    dc_id: int, api_id: int, auth_key: bytes, host: str, port: int
) -> str:
    """Build a WZGram session string from an existing grammers auth key."""
    if len(auth_key) != 256:
        raise ValueError(f"invalid grammers auth key length: {len(auth_key)}")
    addr = host.encode("ascii")[:48].ljust(48, b"\0")
    packed = struct.pack(
        ">BBI?256sQ?H48s",
        3,          # WZ session-string version
        dc_id,
        api_id,
        False,      # test_mode
        auth_key,
        1,          # placeholder user_id; replaced after get_me()
        False,      # is_bot; replaced after get_me()
        port,
        addr,
    )
    return "WZ_" + _encode(packed + struct.pack("<I", zlib.crc32(packed)))


def _read_grammers_session(path: str) -> tuple:
    if not os.path.isfile(path):
        raise FileNotFoundError(f"grammers session not found: {path}")
    uri = f"file:{path}?mode=ro"
    conn = sqlite3.connect(uri, uri=True)
    try:
        row = conn.execute("SELECT dc_id FROM dc_home LIMIT 1").fetchone()
        dc_id = int(row[0]) if row else 2
        row = conn.execute(
            "SELECT ipv4, ipv6, auth_key FROM dc_option WHERE dc_id = ? AND auth_key IS NOT NULL LIMIT 1",
            (dc_id,),
        ).fetchone()
        if row is None:
            row = conn.execute(
                "SELECT ipv4, ipv6, auth_key FROM dc_option WHERE auth_key IS NOT NULL LIMIT 1"
            ).fetchone()
        if row is None or not row[2]:
            raise ValueError("grammers session has no auth_key")
        address = row[0] or row[1] or ""
        host, port = _split_dc_address(address)
        if not host:
            host = _KNOWN_DC_IPS.get(dc_id, _KNOWN_DC_IPS[2])
            port = 443
        return dc_id, bytes(row[2]), host, port
    finally:
        conn.close()


async def _run() -> None:
    upload_path = os.environ["TG_UPLOAD_PATH"]
    api_id = int(os.environ["TG_API_ID"])
    session_path = os.environ["TG_SESSION_PATH"]
    proxy = os.environ.get("TG_PROXY", "").strip() or None
    # Match WZGram's non-Premium default of 12 media sessions. Each session has
    # two part workers, so 12 sessions keep up to 24 x 512 KiB parts in flight —
    # exactly what Telegram recommends for a high-latency/WARP uplink.
    pool = max(4, min(16, int(os.environ.get("TG_WZGRAM_POOL", "12"))))
    rate = max(64, int(os.environ.get("TG_WZGRAM_RATE", "1000")))
    print(f"WZ_INFO:pool={pool} rate={rate}", flush=True)

    # save_file reads these at call time. A high rate removes WZGram's default
    # 32-parts/s pacing for non-premium accounts; the VPS uplink remains the cap.
    os.environ["WZGRAM_UPLOAD_RATE_USER"] = str(rate)
    os.environ["WZGRAM_UPLOAD_RATE_BOT"] = str(rate)
    os.environ["WZGRAM_UPLOAD_RATE_PREMIUM"] = str(rate)
    os.environ["WZGRAM_UPLOAD_POOL_USER"] = str(pool)
    os.environ["WZGRAM_UPLOAD_POOL_BOT"] = str(pool)
    os.environ["WZGRAM_UPLOAD_POOL_PREMIUM"] = str(pool)

    try:
        import uvloop
        uvloop.install()
    except Exception:
        pass

    from wzgram import Client, raw

    dc_id, auth_key, host, port = _read_grammers_session(session_path)
    session_string = _session_string(dc_id, api_id, auth_key, host, port)
    print(f"WZ_SESSION:dc={dc_id} host={host} port={port}", flush=True)

    client = Client(
        "wzgram-upload",
        api_id=api_id,
        api_hash="",
        session_string=session_string,
        proxy=proxy,
        workers=2,
        no_updates=True,
        skip_updates=True,
        timeout=60,
        sleep_threshold=0,
        # save_file opens its own media session pool; this semaphore only gates
        # concurrent save_file() calls, so match the pool instead of leaving the
        # default cap of 1.
        max_concurrent_transmissions=pool,
        auto_no_updates=False,
    )

    size = os.path.getsize(upload_path)
    last_bytes = 0
    last_report = time.monotonic()

    def report(current: int, total: int) -> None:
        nonlocal last_bytes, last_report
        now = time.monotonic()
        if current <= last_bytes:
            return
        if current != total and now - last_report < 0.25:
            return
        last_bytes = current
        last_report = now
        print(f"WZ_BYTES:{current}", flush=True)

    try:
        await client.connect()
        me = await client.get_me()
        client.me = me
        await client.storage.user_id(me.id)
        await client.storage.is_bot(bool(me.is_bot))
        await client.invoke(raw.functions.updates.GetState())
        await client.initialize()

        result = await client.save_file(upload_path, progress=report)
        if result is None:
            raise RuntimeError("WZGram returned no uploaded file")

        if last_bytes < size:
            print(f"WZ_BYTES:{size}", flush=True)
            last_bytes = size

        is_big = result.__class__.__name__ == "InputFileBig"
        md5 = getattr(result, "md5_checksum", b"") or b""
        if isinstance(md5, (bytes, bytearray)):
            md5 = md5.hex()
        print(
            f"WZ_RESULT:{int(result.id)}:{int(result.parts)}:{1 if is_big else 0}:{md5}",
            flush=True,
        )
    finally:
        try:
            await client.stop()
        except Exception:
            pass


def main() -> None:
    try:
        asyncio.run(_run())
    except Exception as exc:
        message = str(exc).replace("\n", " ").replace("\r", " ")[:500]
        print(f"WZ_ERROR:{message}", flush=True)
        sys.exit(1)


if __name__ == "__main__":
    main()

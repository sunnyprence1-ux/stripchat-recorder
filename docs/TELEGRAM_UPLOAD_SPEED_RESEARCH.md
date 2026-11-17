# Fast Telegram video uploads: research and implementation

## Goal

Find the fastest **working** way to upload recorded videos to Telegram from this
project's 1 vCPU / 1 GB VPS, without adding a second Telegram login, without
building wheels/native code on the VPS, and without changing the existing
recording pipeline, topic/cover/caption handling, or aaPanel deployment.

## TL;DR

The fastest compatible path is:

1. Keep the existing Rust/grammers client for login, session storage, topics,
   covers, captions, `sendMedia`, and deletes.
2. Upload only the large file parts through **WZGram** (a Pyrogram fork with
   WarpCrypto) using `save_file()`:
   - **512 KiB parts** — the largest size Telegram accepts.
   - **12 separate media sessions** to the same DC (24 concurrent part workers).
   - **Adaptive pacing** with flood-wait backoff instead of a fixed sleep.
   - **WarpCrypto** for MTProto AES/IGE work.
   - Reuse the **existing grammers auth key**; no second login.
3. Fall back automatically to the Rust uploader if the sidecar fails.

A concrete bug was preventing this path from ever running in production: the
session bridge passed grammers' `dc_option.ipv4` value (for example
`149.154.167.41:443`) as WZGram's `server_address` while also setting port
`443`. WZGram therefore tried to resolve a hostname containing a colon, failed
to connect, and Rust silently became the uploader for every file. That bridge
is fixed in this change.

## What Telegram actually allows

From Telegram's official [upload documentation](https://core.telegram.org/api/files):

- A file is split into equal-size parts.
- `part_size % 1024 == 0`
- `524288 % part_size == 0`

Therefore **512 KiB is the maximum legal part size**. Bigger parts are rejected
with `FILE_PART_TOO_BIG`. Using smaller parts only adds protocol overhead, so
512 KiB is the correct choice.

Additional rules from the same source:

- Files over 10 MB use `upload.saveBigFilePart`; smaller files use
  `upload.saveFilePart`.
- Telegram explicitly recommends a local call queue with **X parts in flight**,
  and further recommends **Y parallel queues on separate TCP connections** to
  the datacenter. X and Y are both tunable.
- The number of parts is limited by `upload_max_fileparts_default` for
  non-Premium accounts and `upload_max_fileparts_premium` for Premium accounts.
  This is another reason not to use smaller parts.
- Flood errors:
  - `FLOOD_WAIT_X` — ordinary rate limit; retry after X seconds.
  - `FLOOD_PREMIUM_WAIT_X` — non-Premium upload throttling, normally only after
    very large cumulative upload volume (tens of GB). Telegram recommends
    offering Premium in this case; no client-side tuning removes it.

## Why the previous path was slow

The old Rust/grammers uploader already used 16 concurrent 512 KiB parts, which
was a big improvement over serial uploads. But it still has two disadvantages
on this deployment:

1. It uses one MTProto session/connection for the part queue, so every part
   competes for the same connection and its congestion window.
2. Python-side WZGram was intended to be the faster path, but the broken
   session-address conversion meant it always failed before sending a single
   part and Rust silently took over. There was no log line saying which engine
   actually ran.

The result looked like "WZGram is slow", but WZGram was never being used.

## Options considered

| Method | Speed potential | Fits this project? | Verdict |
| --- | --- | --- | --- |
| Bot API `sendDocument` | Low for large files; no part-queue control; direct-upload size limit | No (would need a second bot identity and lose topic/cover flow) | Reject |
| Rust/grammers, serial parts | Very low (one RTT per 512 KiB) | Yes | Already replaced |
| Rust/grammers, 16 parallel parts | Good | Yes | Keep as fallback |
| Telethon sequential uploader | Low by default | Would add another library/session | Reject |
| FastTelethon-style patching | Good | Adds a third upload stack and more monkey-patching | Reject |
| TDLib | Very good, robust/resumable | Heavy native C++ build/library and separate session database | Reject for this VPS |
| **WZGram `save_file()`** | **Best compatible option** | **Python sidecar already integrated; reuses grammers auth key** | **Chosen** |

## Why WZGram is the fastest compatible method

WZGram is a Pyrogram fork described by its authors as a high-performance client
boosted by [WarpCrypto](https://wzgram.com/topics/speedups/), a Rust cryptography
extension implementing the AES modes Telegram requires. In the installed
`wzgram==3.1.7` source (`save_file.py`), the uploader:

- uses a **512 KiB part size**;
- creates a pool of **separate media MTProto sessions** for one file
  (`WZGRAM_UPLOAD_POOL_USER`, default 12, max capped by `POOL_SIZE`);
- runs **two part workers per media session**;
- prefetches file parts asynchronously with a read-ahead budget;
- uses an `AdaptiveBucket` pacer that increases its rate on success and backs
  off on `FLOOD_WAIT`;
- retries individual part failures;
- uses WarpCrypto for MTProto AES work, with small packets encrypted inline to
  avoid thread-pool handoff overhead.

That maps almost exactly onto Telegram's own recommendation: X local queue
items plus Y parallel TCP queues. This is why it is the preferred engine here.

## Implementation in this repository

### Upload engine selection

`upload_engine` is a module parameter:

- `wzgram` (default) — use the Python sidecar for files >= 10 MiB.
- `grammers` / `rust` / `builtin` — force the old Rust uploader.

Small files (covers, thumbnails, short clips below 10 MiB) stay on Rust because
starting Python costs more than uploading a few hundred KiB.

### Concurrent parts

- `upload_workers` defaults to **16** and is capped at 16.
- Rust passes `workers.clamp(4, 12)` as `TG_WZGRAM_POOL`, so the default becomes
  **12 media sessions**.
- WZGram runs 2 part workers per session, giving up to **24 concurrent 512 KiB
  part uploads** — matching WZGram's own non-Premium default pool.
- Read-ahead is raised to 32 chunks (16 MiB) so the part workers are not starved
  by disk reads.
- The adaptive pacer is set to a high ceiling (1000 parts/s) so the VPS uplink,
  not a client-side sleep, is the limiter. Flood waits still back off normally.

### Session reuse

The sidecar reads the existing grammers SQLite session:

- `dc_home.dc_id` selects the home DC.
- `dc_option.auth_key` is the already-authorized 256-byte auth key.
- `dc_option.ipv4` is split into a bare host and port before being encoded into
  a WZ v3 session string.

Rust still owns all Telegram business logic. After WZGram uploads the parts, it
prints:

- `WZ_BYTES:<current>` — feeds Rust's progress counter.
- `WZ_RESULT:<file_id>:<parts>:<is_big>:<md5>` — lets Rust build
  `InputFile`/`InputFileBig` and send the media exactly as before.
- `WZ_ERROR:<message>` — triggers Rust fallback.

### Observability

New log markers make the active engine obvious:

- `UPLOAD_ENGINE: wzgram (...)`
- `UPLOAD_ENGINE: rust (...)`
- `UPLOAD_ENGINE: rust (fallback from wzgram)`
- `WZ_SESSION:dc=... host=... port=...`

If the VPS logs show `fallback`, the WZGram path failed and no amount of
worker tuning will help until that error is fixed.

## Verification

Local checks added in this change:

- Python stdlib regression tests for IPv4/IPv6 socket-address splitting, WZ v3
  session-string encoding/CRC, and grammers SQLite parsing.
- CI runs these tests in the Docker builder stage.
- The generated session string was decoded with WZGram 3.1.7 itself: it reports
  `server_address=149.154.167.41`, `port=443`, and the identical auth key.

After deployment, run one real large upload and collect:

```text
UPLOAD_ENGINE:
WZ_INFO:
WZ_SESSION:
STATUS:
PROGRESS:
WZ_BYTES:
chunk ... attempt ...
WZGram upload failed
FLOOD_WAIT
FLOOD_PREMIUM_WAIT
```

Compute observed throughput as `file_size / elapsed_seconds`. Compare with
`upload_engine=rust` on the same file only if WZGram still loses.

## What can still cap speed

If WZGram is confirmed active and throughput is still poor, the remaining
bottlenecks are outside the upload queue:

1. **WARP proxy bandwidth/latency** (`socks5://127.0.0.1:40000`). All media
   sessions share this proxy. Test a direct Telegram route or a different WARP
   endpoint/WARP+ if available.
2. **Telegram account throttling** — `FLOOD_PREMIUM_WAIT_X` means the account,
   not the code, is being rate-limited. Telegram Premium raises this limit.
3. **File part cap** — `upload_max_fileparts_default` limits total size unless
   Premium is used; 512 KiB parts already maximize bytes per part.
4. **VPS disk/CPU** — 1 vCPU must feed 24 encrypting workers. WarpCrypto keeps
   this manageable, but a very slow virtual disk can still starve read-ahead.
5. **Server-side video processing** — for videos sent to large chats, Telegram
   may return a scheduled message first and convert the video afterwards. The
   upload can be finished while the message appears delayed.

## References

- Telegram upload protocol: <https://core.telegram.org/api/files>
- `upload.saveBigFilePart`: <https://core.telegram.org/method/upload.saveBigFilePart>
- WZGram: <https://github.com/rjriajul/wzgram>
- WZGram speedups/WarpCrypto: <https://wzgram.com/topics/speedups/>
- WZGram docs: <https://wzgram.com/>
- Installed source used for analysis:
  `/tmp/opencode/wzvenv/lib/python3.12/site-packages/pyrogram/methods/advanced/save_file.py`
- grammers session storage format (confirming `ipv4` is a full socket address):
  `grammers-session-0.9.0/src/storages/sqlite.rs`

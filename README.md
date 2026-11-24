# Signal Room

Signal Room is a self-hosted Stripchat recorder maintained by **sunnyprence1-ux**. Use the English web or desktop app to monitor live rooms, record streams, manage finished videos, and automate post-processing.

## Quick start with Docker Compose

Install Docker and the Docker Compose plugin, then run:

```sh
git clone https://github.com/sunnyprence1-ux/stripchat-recorder.git
cd stripchat-recorder
docker compose pull
docker compose up -d
```

Open **http://localhost:3030** on the server, or **http://SERVER_IP:3030** from another device on your network. Set the recording directory to `/app/stripchat-recorder/recordings` in the setup wizard to keep recordings in the mounted host folder. Leave optional network settings blank unless your server needs them.

Compose uses `ghcr.io/sunnyprence1-ux/stripchat-recorder:latest`. If the registry requires authentication, run `docker login ghcr.io` with credentials that can read the package. Keep the management interface on a trusted network or behind an authenticated reverse proxy.

| Task | Command |
| --- | --- |
| Start or recreate the service | `docker compose up -d` |
| Check service status | `docker compose ps` |
| Follow logs | `docker compose logs -f --tail=100` |
| Update to the latest published image | `docker compose pull && docker compose up -d` |
| Restart the running service | `docker compose restart` |
| Stop and remove the container | `docker compose down` |

Configuration, recordings, modules, and logs are stored under `./data/` on the host. Back up that directory before changing servers. If you choose a separate final video directory, make sure it also points to a mounted location. To set the container timezone, add a line such as `TZ=Asia/Kolkata` to a `.env` file next to `docker-compose.yml`, then recreate the service.

## Find rooms and record

1. Open **Watchlist → Add Streamer → Browse live rooms** in the web app. Browse portrait thumbnails, search by username, select a country, or hide rooms already tracked. Selecting a country searches additional directory pages automatically. Results depend on the country information supplied by the directory.
2. Tap **Add** on a room. **Add manually** also accepts a username or room URL. The desktop app supports manual entry.
3. Select **Record** beside a live, recordable room, then **Stop** to finish it. Open the room’s options and enable **Auto** to record when it becomes available. The Watchlist shows live and recording counts and filters. The web app also offers bulk start and stop controls.
4. In **Settings → Recording**, configure the maximum concurrent recordings, live status polling, preferred resolution and fallback, and merge format. Set **Recording File Duration** to `1800` for roughly 30-minute files or `0` for unlimited duration. Video rotation follows whole segment boundaries.

**Finder** also offers name and image search through its external search provider. Open Finder, select the search method, and use **Add** on an eligible Stripchat result. Image search sends the selected image to the search provider.

## Manage finished videos

Open **Library** to see active recordings, merge progress, finished files, and available disk space. Expand a streamer’s group to browse its files. Use the row controls to play or preview a recording where available, run post-processing, or delete a file. Select multiple finished recordings for batch actions. The web library can also send selected recordings directly to Telegram.

Use **Settings → Recording** to set a separate **Final Video Directory** if you want merged videos stored apart from source segments and metadata.

## Configure post-processing

Open **Post-processing → Add Module**, choose a module, then expand **Module settings** and fill in its options. Modules run in the displayed order after a recording merges. Use the arrow controls to reorder modules, **Skip** to disable a stage, and **Test** if a module supports it. Telegram’s test can post to the destination you configured.

| Module | How to use it |
| --- | --- |
| Short video filter | Set the minimum duration. Enable preview mode to see what would be filtered before allowing deletion. |
| Discord notification | Enter your Discord webhook URL and choose the available display or proxy options as needed. |
| Telegram | Configure the destination and sign-in method using the steps below. |
| Google Drive | Open the module’s **Login** flow, sign in with Google, and complete the verification prompt. Configure the destination in the module settings. |

See the [module development guide](docs/module-development.en.md) for custom modules.

### Telegram uploads

1. Add the **Telegram** module. Enter your API ID and use **Login** to sign in with a user account, or configure the bot credentials and select bot mode. **Auto** prefers an existing signed-in user session.
2. Enter the target **Chat ID**. You can also supply the username for a public group. For a private group, leave the username blank. To organize uploads into one topic per streamer, enable topics in the Telegram supergroup and grant the sending account permission to manage topics.
3. Choose whether to send video, a cover image, and a caption. Videos above **2 GB** are prepared as numbered parts and sent in order to the same destination. When captions are enabled, multipart uploads include part numbers.
4. For a one-time send in the web app, open **Library → Send to Telegram** on a finished recording, or select several files and use the batch action. The send dialog lets you override settings for that upload. Review **Delete after sending** if you want to keep local copies.

## Relay and network settings

- **HLS relay (web only)** — Open `http://SERVER_IP:3030/stream/USERNAME` in an HLS player. The **Relay** page shows active sessions and client connections.
- **Network** — Use **Settings → Network** to set separate API and CDN proxies or a Stripchat mirror when needed.
- **Mouflon settings** — Open **Settings → Mouflon Decryption Keys** to manage keys you are authorized to use. Fresh installs have no default sync URL. If you have an authorized sync endpoint, enter its URL and optional token, then press **Sync Now**. Back up `./data/config/mouflon_keys.json` privately when moving a Docker installation.

## Desktop and source builds

See the [desktop guide](desktop/README.md) for the Tauri app and the [deployment and usage guide](README.en.md) for source builds, module settings, and advanced Telegram options.

## License and provenance

Signal Room is a modified distribution of [ChanTrail/StripchatRecorder](https://github.com/ChanTrail/StripchatRecorder). The fork's changes include the Signal Room interface, live room browser, recording updates, and Telegram upload handling. This modification notice was updated on 2026-11-24. See [LICENSE](LICENSE) for the GNU General Public License v3.

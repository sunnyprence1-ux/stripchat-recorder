# Signal Room

Signal Room is a self-hosted Stripchat recorder maintained by **sunnyprence1-ux**. The English web and desktop apps let you watch live status, manage recordings, and send finished videos through a configurable post-processing workflow.

## What you can do

- Browse live rooms with portrait thumbnails, filter by country or username, and add rooms directly to the watchlist.
- Start recordings manually or automatically, monitor concurrent sessions, and rotate long recordings into separate files.
- Manage recordings from the library and configure processing modules for Telegram, Discord, Google Drive, and short-video filtering.
- Send videos to Telegram groups or topics. Videos over 2 GB are split into numbered parts before uploading.
- Use the responsive browser interface or the Tauri desktop app.

## Run with Docker

From a checkout of this repository:

```sh
docker compose pull
docker compose up -d
```

Open [http://localhost:3030](http://localhost:3030) and complete initial setup. Compose uses the image published by this repository to GitHub Container Registry. If pulling requires authentication, sign in to GHCR with an account that can read the package.

The Compose file mounts configuration, recordings, logs, and modules from `./data/`. Back up this directory before moving servers. The optional Mouflon sync URL starts blank on fresh installs. Configure a sync endpoint you have access to, or use your locally saved settings.

For configuration, manual uploads, and source builds, see the [deployment and usage guide](README.en.md). For the native app, see the [desktop guide](desktop/README.md).

## License and provenance

Signal Room is a modified distribution of [ChanTrail/StripchatRecorder](https://github.com/ChanTrail/StripchatRecorder). The fork's changes include the Signal Room interface, live room browser, recording updates, and Telegram upload handling. This modification notice was updated on 2026-11-24. See [LICENSE](LICENSE) for the GNU General Public License v3.

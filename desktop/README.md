# Signal Room Desktop

The desktop build uses the same recording backend and post-processing modules as the web build. It communicates through Tauri IPC and runs in a native window. HLS relay and live-room directory browsing are available in the web edition.

## Development

Install Rust, Node.js with npm, FFmpeg on your PATH, and the [Tauri system prerequisites](https://tauri.app/start/prerequisites/). From the repository root:

```sh
npm run dev:desktop
```

## Release build

```sh
npm run build:desktop
```

The new desktop application identifier is `io.github.sunnyprence1ux.signalroom`. The recorder locates its `config/` directory relative to the executable runtime directory. Back up that directory before upgrading or moving an existing installation. Copy its contents into the new runtime's `config/` directory if you change installation locations.

See the [project overview](../README.md) for deployment and license information.

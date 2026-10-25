# Syncra

Ultra-low-latency remote desktop and game streaming for Windows, written in Rust.

- All-GPU host path: DXGI Desktop Duplication → NVENC (sliced, low-latency tuning), no CPU readback.
- Native client: NVDEC → D3D11 render, tearing/VRR present, local cursor overlay.
- Custom UDP transport: fragmentation, Reed-Solomon FEC, NACK, pacing, congestion control, Noise handshake + AES-GCM.
- Keyboard, mouse and gamepad (ViGEm) input; Opus audio over WASAPI.
- Connect by ID: the host shows a 9-digit ID, the client enters it, the host approves. No accounts.
- Peer-to-peer with UDP hole punching, relay fallback through a small signal server that never sees decrypted media.

Design, latency budget and measured results: [ARCHITECTURE.md](ARCHITECTURE.md).

## Requirements

- Windows 11 with an NVIDIA GPU and a recent driver (NVENC / NVDEC are loaded from the driver at runtime).
- Rust (stable, MSVC toolchain) and LLVM/libclang (for bindgen).
- [ViGEmBus](https://github.com/nefarius/ViGEmBus) on the host for gamepad support.

## Build

```bash
cargo build --release
cargo test --workspace
```

The release build uses `target-cpu=native` (`.cargo/config.toml`), so don't copy those binaries to other machines. For a portable installer (AVX2, static CRT, ViGEmBus, firewall rule), use `deploy/windows/build-setup.sh` (needs Inno Setup 6; see the script header).

## Signal server

Syncra needs a signal server (rendezvous + relay, UDP port 7000) for connect-by-ID. None is built in: run your own.

```bash
# On any Linux box with Docker and a public IP:
deploy/signal/bundle.sh        # -> target/signal-bundle.tar.gz, copy it over
docker compose up -d --build   # inside the unpacked bundle
# or directly:
cargo run --release -p signal -- --listen 0.0.0.0:7000
```

Point the apps at it, either:

- at build time: `SYNCRA_SIGNAL=your.server:7000 cargo build --release` (becomes the default), or
- per machine: `%LOCALAPPDATA%\Syncra\settings.txt` with the line `signal=your.server:7000`.

## Run

- `syncra.exe` – one window: shares this PC (shows its ID) and connects to others.
- `host.exe` / `client.exe` – single-purpose versions with command lines:

```bash
host --signal your.server:7000
client --id "482 913 077" --signal your.server:7000
# direct, no signal server (LAN / loopback test):
host --listen 127.0.0.1:9000
client --connect 127.0.0.1:9000
```

## Layout

| Crate | Role |
|---|---|
| `proto` | wire headers, input messages, clock, crypto session |
| `net` | UDP socket, fragment/reassembly, FEC, NACK, pacing, congestion control |
| `nvenc-sys` / `nvdec-sys` | bindgen FFI, loaded at runtime from the driver DLLs |
| `host` | DXGI capture → NVENC, input injection, ViGEm gamepad |
| `client` | window, NVDEC → D3D11 render, HUD, input capture |
| `signal` | rendezvous + relay server |
| `audio` | WASAPI capture/render, jitter buffer, resampler |
| `opus-sys` | vendored libopus built with `cc` |
| `probe` | headless Linux test receiver |
| `ui` | native Win32 forms |
| `app` | `syncra.exe` |
| `bench` | click-to-photon and gamepad latency harness |

## License

MIT, see [LICENSE](LICENSE). Vendored code in `third_party/` keeps its own license: libopus (BSD-3-Clause), nv-codec-headers (MIT).

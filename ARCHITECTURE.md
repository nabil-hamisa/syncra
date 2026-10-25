# Architecture

Ultra-low-latency remote desktop and game streaming for Windows, written in Rust.
The goal is to beat Parsec on end-to-end latency and on resilience over real internet links.

- **Host**: Windows 11 with an NVIDIA GPU (NVENC). It captures, encodes and streams.
- **Client**: native Windows app (NVDEC, with a D3D11VA fallback). It decodes, renders and sends input.
- **Signal server**: tiny rendezvous and relay service. It never sees decrypted media.

## Latency targets

| Stage | Budget (1080p120, loopback) |
|---|---|
| DXGI capture + GPU copy | ~0.5–1 ms |
| NVENC encode (sliced, overlaps send) | 2–3 ms |
| Packetize + FEC + encrypt + send | <0.3 ms |
| Network | 0 (loopback) / RTT/2 (internet) |
| NVDEC decode (overlaps receive) | 1–2 ms |
| Convert + present (tearing/VRR) | 0–1 ms |
| **Total p95** | **<8 ms + RTT/2** |

## High-level data flow

```
HOST (all GPU, no CPU readback)                    CLIENT
DXGI Desktop Duplication ─► GPU copy ─► NVENC      recv thread (busy-poll, URO)
  (P1, ULL tuning, CBR, VBV≈1 frame, no B,          → decrypt → slice reassembly → FEC repair
   infinite GOP, intra refresh, N slices,           → NVDEC (per slice as available)
   sub-frame readback)                              → CUDA/D3D11 interop → NV12→RGB shader
  each slice → fragment ≤1200B → RS-FEC             → flip swapchain Present(0, tearing/VRR)
  → AES-GCM → paced UDP (USO) ════ P2P/relay ═══►  → local cursor overlay
SendInput / ViGEm ◄═══ input socket (redundant) ═══ Raw Input + XInput (1 kHz)
WASAPI loopback → Opus 5ms ═════════════════════►  Opus → WASAPI low-latency
Bitrate controller ◄═══ feedback (arrival deltas, loss, RTT) every ~5ms ═══

             signal server: ID registry, connect requests, candidate exchange, UDP relay
```

## Workspace

| Crate | Kind | Responsibility |
|---|---|---|
| `proto` | lib | Wire headers, message enums, rendezvous protocol (`proto::rendezvous`), host ID derivation |
| `net` | lib | UDP socket (USO, encryption + relay wrapping), Noise_XX + AES-GCM (`net::crypto`), fragmenter/reassembler, Reed-Solomon FEC, NACK, congestion controller, link simulator |
| `nvenc-sys` | lib | FFI to `nvEncodeAPI.h` (loaded from `nvEncodeAPI64.dll`) |
| `nvdec-sys` | lib | FFI to `nvcuvid.h` + CUDA driver API (loaded from `nvcuda.dll` / `nvcuvid.dll`) |
| `host` | bin | Capture, encode, input injection, gamepad emulation, audio capture, approve UI |
| `client` | bin | Window, decode, render, HUD, input/gamepad capture, audio playback, connect UI |
| `signal` | bin | UDP rendezvous (ID registry, introductions) + UDP relay; portable (no Windows deps) |
| `bench` | bin | Click-to-photon latency harness and Parsec comparison |

Third-party headers live in `third_party/` (see `third_party/README.md`).

## Host pipeline

### Threads
| Thread | Priority | Work |
|---|---|---|
| `capture+encode` | MMCSS "Games", high | `AcquireNextFrame` → `CopyResource` into an NVENC-registered texture → `nvEncEncodePicture` |
| `slice-send` | high | Waits on NVENC sub-frame completion, then fragments, applies FEC, encrypts and paces each slice onto the socket |
| `net-rx` | MMCSS Games | Blocks on the socket. Input packets → one batched `SendInput` per datagram, plus a ViGEm report for each pad with a new state, **the moment they arrive**. Control messages → flags the encode loop reads (keyframe / cursor shape requests, peer). Drops a client that is silent for 3 s (the client sends a keepalive every 500 ms), releasing everything it pressed and unplugging its pads |
| `gamepad-plug` | normal | Plugs new ViGEm pads. Plugging waits on PnP (seconds on the first plug after a driver install), so it never runs on `net-rx`; states for a pad that is still plugging are dropped (the client re-sends full state every 50 ms) |
| `control` | normal | Later: feedback, bitrate decisions, NVENC reconfigure, keepalive, signaling |
| `audio` | MMCSS "Pro Audio" | WASAPI loopback → Opus → send |

Stages hand work to each other through lock-free SPSC ring buffers (`rtrb`). The hot path does no heap allocations: buffers come from preallocated pools.

### Capture
- DXGI Desktop Duplication on the selected output. Frames stay in GPU memory.
- The frame is `CopyResource`'d into one of a small ring of BGRA textures registered with NVENC, and the duplication frame is released immediately.
- Pointer shape and position are taken from the duplication metadata and sent as a separate message. They are never burned into the video.
- Capture runs at the display refresh rate (up to 240 Hz). When nothing changes, nothing is encoded or sent.
- Optional **input-triggered capture**: right after injecting input, the host polls for the next frame immediately instead of waiting.

### Encode (NVENC)
- Preset P1, tuning `ULTRA_LOW_LATENCY`, rate control CBR, VBV buffer ≈ one frame, no B-frames.
- Infinite GOP with **intra refresh**, so there are no periodic I-frame spikes.
- **Slice mode + sub-frame readback** (`enableSubFrameWrite`): each slice's bitstream is sent as soon as it's written, so network transmission overlaps encoding.
  - Readback polls without waiting, then always finishes with one **blocking** lock, which emits the last slice and the end-of-picture mark as one chunk (no separate tiny terminator chunk for a burst to kill). The driver can report a picture complete before `bitstreamSizeInBytes` includes the last slice. Until Phase 5, that dropped the bottom slice of roughly 1 picture in 1,300; it was found because NVDEC flagged the picture concealed while FFmpeg silently concealed it.
- **Level 5.1 with 16 reference frames**, so reference invalidation can skip a lost frame for RTTs up to ~130 ms at 120 fps (level 4.x allows only 4 at 1080p).
- Loss recovery uses `nvEncInvalidateRefFrames` on every frame from the first lost one to the newest: in IPPP all of them are tainted. The next frame is flagged `RECOVERY`, with no keyframe.
- Bitrate changes via `nvEncReconfigureEncoder` without resetting the stream.
- Codec order: H.264 first, then HEVC and AV1 (better quality per bit means less time on the wire).

## Client pipeline

### Threads
| Thread | Work |
|---|---|
| main (UI) | Owns the window and runs the blocking message loop **only**. Windows can stall a window's thread inside message dispatch (drags, focus changes): measured 90 ms stalls before this split. Talks to `stream` through atomics and posted messages |
| `stream` (MMCSS Games) | Busy-polls the socket, reassembles chunks (FEC repair, NACKs), feeds each one to NVDEC as it completes, maps the output through CUDA→D3D11 interop, runs the NV12→RGB shader, and calls `Present`, all on one thread with no hand-off. Also sends feedback every 10 ms and answers pings. Later: decrypt |
| main (UI), input | Window messages are turned into input events and sent immediately from the window procedure (absolute mouse, buttons, wheel, scancodes; raw-input deltas in relative mode) |
| `gamepad` (MMCSS Games) | XInput polled every 1 ms on absolute deadlines → full pad state on change and every 50 ms |
| `audio` | Opus decode → adaptive jitter buffer → WASAPI |

### Decode and present
- NVDEC with `ulMaxDisplayDelay = 0`. Decoded surfaces are shared with D3D11 with zero copy.
- D3D11VA path for non-NVIDIA clients.
- Flip-discard swapchain, `SetMaximumFrameLatency(1)`, waitable object, `DXGI_PRESENT_ALLOW_TEARING`. Uses VRR/G-Sync when available.
- Frames older than the newest complete frame are dropped. The client never queues video.
- Every picture's NVDEC status is checked (`cuvidGetDecodeStatus`). A picture with an error or concealment is never shown and is reported as a loss.
- After a loss, frames keep being decoded (so the reference chain stays intact) but are **not shown** until a keyframe or a `RECOVERY` frame whose `lost_acked` covers the loss arrives. The screen holds the last clean frame instead of showing artifacts.
- The cursor is drawn locally at the client's own input position, so it has zero network latency.

## Network protocol

Custom protocol over UDP. Every datagram is ≤ 1200 bytes of payload so it survives typical internet MTUs without fragmentation.

### Packet header (44 bytes LE, `crates/proto`; encrypted with the payload once a session is up)
```
 0 u8  version        1 u8  channel (0 video, 1 audio, 2 input, 3 control, 4 cursor, 5 cursor shape)
 2 u16 flags          (LAST_CHUNK, KEYFRAME, RECOVERY, RETRANSMIT)
 4 u32 sequence       per channel, later also the AEAD nonce counter
 8 u32 frame_id
12 u16 chunk_id       one or more NVENC slices, in bitstream order
14 u16 frag_index    16 u16 frag_count (data shards)    18 u16 parity_count (Reed-Solomon shards)
20 u32 chunk_len
24 u64 capture_us     DWM present time of the captured image (DXGI LastPresentTime, QPC µs)
32 u64 send_us        host clock at send, for latency and queuing-delay estimation
40 u32 lost_acked     newest lost frame the host has excluded from references
```
Datagrams are ≤1200 bytes total. A chunk's datagrams are all the same size (the last data shard is padded) and go out with **one USO syscall**. A reassembler releases chunks strictly in order, as soon as each completes, directly or rebuilt from parity.

### Reliability
Three layers, cheapest first:
- **FEC** (`net::fec`): Reed-Solomon per chunk. GF(2^8) Cauchy (`reed-solomon-erasure`, SIMD) for chunks of up to 256 shards, which is every chunk at 1080p. Leopard GF(2^16) (`reed-solomon-simd`) beyond that. Both ends pick by `k + m`.
  - Measured decode cost: GF(2^8) 0.6–28 µs at our chunk sizes, versus 226–308 µs for Leopard, whose FFT has a fixed cost. Switching removed ~0.3 ms from every repaired frame.
  - Shards are sized to the chunk, so a 300-byte slice gets 300-byte parity. Any `k` of the `k+m` shards rebuild it.
  - The parity ratio is `5% + 2.5 × recent loss peak` (2 s half-life), capped at 60%.
  - The per-chunk floor grows with loss: 1 shard on a clean link, 2 at a few %, and 3 for tiny chunks under heavy loss. Losses are bursty, and losing the tiny end-of-frame chunk loses the frame.
- **NACK** (`net::history`): when packets of a later chunk arrive while an earlier chunk is still short, the client requests the missing data shards with a bitmap.
  - The host keeps 200 ms of sent chunks and resends from `net-rx`, flagged `RETRANSMIT`.
  - NACKs fire once later data has arrived. A time-based "chunk went quiet" trigger was tried at 0.5 ms and 3 ms and lost more frames (46–53 vs 33–37 per 12 s at 10% bursty loss), because it spent the chunk's one NACK too early.
  - The frame waits at most 1.5 × RTT + 1 ms, and NACK is enabled only while RTT < 20 ms; above that a resend would arrive later than a recovery frame. The host shares its RTT estimate inside `Ping`.
- **Reference invalidation**: an unrecoverable frame is reported (`FrameLost` with the first lost id, repeated every 30 ms).
  - The host invalidates every tainted reference, so frames after the loss predict only from frames the client has.
  - It falls back to an IDR only when the taint reaches past the 16-frame window.
  - IDRs, requested or loss-driven, are rate-limited to one per max(250 ms, 3 × RTT). The client's keyframe timeout also scales with RTT (≥ 4 × RTT). A fixed timeout shorter than the round trip caused a keyframe storm that buried a 10 Mbps link.
- **Input channel**: every packet carries the last N events, and gamepads also send a full-state snapshot every 50 ms. Input is never retransmitted, only made redundant.

### Congestion control (`net::cc`, `net::feedback`)
- **Signal**: every 10 ms the client reports expected/received datagrams (from sequence numbers, retransmits excluded), bytes, and **queuing delay**.
  - Queuing delay is one-way delay minus its windowed minimum, so clock offset cancels out and no clock sync is needed.
  - The controller uses the **per-interval minimum**: every frame leaves as a burst, so the average contains our own serialization delay, while a burst's first datagram only sees a standing queue.
- **Control**: delay-based.
  - Queue above 5 ms: cut to 85% of the *received* rate, at most once per RTT + 50 ms.
  - Queue below 2.5 ms: ramp 10% per 100 ms, or 3% near the rate that last caused trouble.
  - These thresholds were A/B tested against 4/8 ms. On a 10 Mbps / 30 ms link, the standing queue went from 3.2 to 1.2 ms and p50 from 29.8 to 28.0 ms, both stable with zero loss.
  - In between: hold.
  - Past the last trouble rate, the ramp is capped at 1.5 × the rate actually flowing, so an application-limited sender can't ramp to a target it has never tested.
- **Random loss does not cut the rate**: only loss above 30% counts as congestion (policer); FEC handles the rest.
  - **Congestion loss does not raise FEC**: drops while a queue is building are the bottleneck overflowing, and parity would feed the spiral.
- **Budget**: the encoder gets `target / measured wire overhead`, where overhead covers headers and all parity, so video plus FEC fits the target.
- **Adaptive frame rate**: if NVENC keeps producing more than it was asked for for 2 s (its frames are at their floor), the host encodes every 2nd or 4th frame. The encoder bitrate is scaled so the per-frame budget stays right, and the rate steps back up only at 2.5 × the target that forced it down.
- Not yet: send pacing within a frame. The bottleneck tests run without it, and bursts are absorbed by the min-delay signal.

### Adaptive resolution (`host::scale`, `Stream::pick_resolution`)
- On a slow link, fewer pixels at full frame rate look far better than 1080p starved of bits. Ladder: capture size, ⅔, ½, ⅓ (1080p → 720p, 540p, 360p).
- The host downscales each captured frame on the GPU (pixel shader, box filter of `ceil(scale)²` bilinear taps over each output pixel's footprint, so 2× and 3× are exact box filters) into per-level BGRA rings registered with NVENC. Lower levels encode faster even including the downscale: 720p 0.89–1.11 ms p50 vs 1.59–1.67 ms at 1080p.
- A switch is `nvEncReconfigureEncoder` with reset + IDR. The IDR is flagged as a keyframe, counts against the IDR rate limit, and marks every earlier frame as excluded from references (loss reports from before the reset are moot). If NVENC refuses, streaming continues at the old size.
- The client reconfigures NVDEC in place (`cuvidReconfigureDecoder`; the decoder is created with room for 1080p). Switch freeze went from ~49 ms (destroy + create) to none. Mouse and cursor positions are normalized, so input mapping doesn't care.
- Decision, from the congestion target divided by the measured wire overhead (video vs video):
  - a level needs 0.04 bits per pixel at the configured fps (at 120 fps: 1080p ≈ 10 Mbps, 720p ≈ 4.4, 540p ≈ 2.5, 360p ≈ 1.1);
  - the session opens at the level the slow-start rate supports (360p) and, while slow start is still probing, climbs as soon as a level fits with 30% margin (300 ms between steps): a LAN is at 1080p ~1.1 s after connecting, and a slow link never sees a full-size IDR;
  - afterwards: down after 1 s below need (straight to the level that fits), up after 3 s above the next level's need × 1.3, never two switches within 2 s;
  - resolution drops before frame rate: a switch resets the fps divisor, and IDR frames are left out of its overshoot test.
- `--no-adaptive-res` keeps the capture size.

### Socket tuning (Windows 11)
- USO **per message**: `WSASendMsg` with a `UDP_SEND_MSG_SIZE` control message. A socket-wide segment size would be shared by every clone of the socket and could split another thread's datagram. URO (`UDP_RECV_MAX_COALESCED_SIZE`) is not used yet.
- Large `SO_SNDBUF`/`SO_RCVBUF`, DSCP EF marking via qWAVE.
- `timeBeginPeriod(1)` for timer precision.

### Encryption (`net::crypto`, applied inside `net::Socket`)
- `Noise_XX_25519_AESGCM_SHA256` handshake (`snow`, ring backend) using each side's static X25519 key, generated once and stored in `%LOCALAPPDATA%\Syncra\{host,client}.key`.
- Transport: AES-256-GCM per datagram with an **explicit 64-bit nonce** (`StatelessTransportState`), because UDP reorders and drops. Wire format `[0xB0][nonce u64][ciphertext + 16-byte tag]`, 25 bytes overhead. A 1024-wide sliding window rejects replays; forged datagrams are dropped.
- The socket seals every application datagram on send (including USO batches: each datagram is sealed in place, so the segments stay equal-sized and are sealed one ~60 KB batch at a time) and opens on receive. Video, input, FEC and NACK code are unchanged.
- Rendezvous datagrams (`0xC0`) and handshake datagrams (`0xA0`) pass through unsealed; application datagrams start with the protocol version, so the first byte tells them apart. While a link requires a session, unsealed application datagrams are dropped.
- `--listen` / `--connect` (direct, no signal server) stay unencrypted for loopback benchmarks.

## Connection flow (no accounts)

```
Host                         Signal server (UDP)                 Client
 |-- REGISTER(pubkey, LAN addr) ->|  every 5 s                       |
 |<-- REGISTERED(id, public addr)-|                                  |
 |   (logs "host ID: 482 913 077")|                                  |
 |                                |<-- CONNECT(id, pubkey, name) ----|  (client --id 482913077)
 |<-- INCOMING(session, ...) -----|   (repeated until answered)      |
 |   [Approve / Deny popup]       |                                  |
 |-- ANSWER(accept) ------------->|-- ACCEPTED(host key, addrs) ---->|  client checks host_id(key) == typed ID
 |<========== PUNCH / PUNCH_ACK to public + LAN addresses ==========>|
 |<========== or RELAY [0xC7][session][datagram] via server ========>|  after 1.5 s, or --force-relay
 |<========== Noise_XX handshake (host verifies approved key) ======>|
 |<========== sealed Hello, then the encrypted stream ==============>|
```

- Rendezvous runs over the same UDP socket as the stream, so the server sees each side's NAT mapping and the mapping stays open. No STUN or WebSocket.
- The 9-digit ID is the host's public key mod 10^9, so it stays the same across restarts. The client checks that the authenticated key maps to the ID it typed: a malicious signal server can deny service, but it can't substitute a host or read the stream.
- Nothing streams until the host user clicks **Yes**. "Always allow" stores the client's key in `trusted_clients.txt`. The popup shows the key fingerprint first, and the name (unverified, control characters stripped) second. One popup at a time. `--auto-approve` is for tests.
- The signal server keeps only soft state (hosts expire after 30 s, sessions after 120 s idle) and needs no database. It relays only for sessions the host accepted, and only between the two registered endpoints. Relayed traffic is end-to-end encrypted.
- A newly authenticated client replaces the current one. When a client leaves, the host goes back to accepting handshakes only.
- Usage:
  - `signal --listen 0.0.0.0:7000`
  - `host --signal SERVER:7000`
  - `client --id "482 913 077" --signal SERVER:7000 [--force-relay]`

## Windows (GUI)

Native Win32 controls (`ui` crate: forms, fonts, DPI scaling, clipboard), no UI framework; nothing on the stream path depends on them. The executables embed a manifest (Common Controls v6 and per-monitor-v2 DPI awareness from process start, which desktop duplication needs).

- **Installer** (`deploy/windows/build-setup.sh` → `target/setup/Syncra-setup-<version>.exe`, Inno Setup 6; `VIGEM` points at the ViGEmBus installer): a portable `Syncra.exe` (cargo bin `syncra`) built for `x86-64-v3` with a static C runtime (the dev build is `target-cpu=native`, which may use AVX-512 and crash on other PCs, and needs `VCRUNTIME140.dll`). It installs to Program Files, adds Start menu and optional desktop shortcuts, adds an inbound firewall rule for the app (removed on uninstall), installs ViGEmBus 1.22 if it's missing (optional task), and warns when there's no NVIDIA driver (`nvEncodeAPI64.dll` / `nvcuda.dll`).
- **`Syncra.exe` (the app, `crates/app`, bin `syncra`):** one window for both sides, like Parsec. Windows, the installer and the data folder are named Syncra; the folder used before the rename (`parsec-rs`) is moved over on first start, so the ID and trusted devices are kept.
  - *This PC*: sharing starts with the app. It shows the ID with Copy ID, the status and live stats, and approval prompts are owned by this window.
  - *Connect to a PC*: ID + Connect (server field below, remembered). The connect flow runs on a worker thread, then the stream window opens on its own thread, so you can share and watch another PC at the same time. While connected, Connect reads **Disconnect** (closes the stream window); closing the stream window has the same effect. Closing the app stops sharing.
  - **Disconnect** under *This PC* (and on the host panel) ends the session of whoever is watching: the host sends `Bye` three times while the session keys still exist, then drops the peer, and the client closes its stream window on `Bye` ("the host ended the session"). The device stays trusted and can connect again.
  - `host` and `client` are libraries (`host::share`, `client::stream_session`) with thin `host.exe` / `client.exe` wrappers, which keep their command lines and single-purpose windows.
- **Host:** `host.exe` with no arguments (or `--gui`) shares this PC through the saved server and opens a panel:
  - the 9-digit ID in large type with a **Copy ID** button;
  - status: connecting, "Ready", server unreachable, a device asking, "Connected: <name> (direct / relayed via server)";
  - live stats once a second: fps, resolution, bitrate, RTT.

  Approval prompts are owned by the panel. Closing the panel stops sharing: the encode loop sees the flag and the host shuts down normally.
- **Client:** `client.exe` with no arguments (or `--gui`) opens a launcher with **Host ID**, **Server** (remembered in `%LOCALAPPDATA%\Syncra\settings.txt`, default: `SYNCRA_SIGNAL` at build time, else none) and **Connect**.
  - The connect flow (lookup, approval, hole punch, handshake) runs on a worker thread while the launcher shows progress; errors appear inline and the form is usable again.
  - The stream window opens only once the session is up.
- **Double-click runs:** started this way, Windows gives the program a console of its own. It is released, and logs go to `%LOCALAPPDATA%\Syncra\host.log` / `client.log` through a non-blocking writer thread. Fatal errors appear in a message box. Run from a terminal, logging stays on the terminal.
- **Same machine:** a client whose host is on the same machine turns audio playback off (it would feed back into the host's capture) and gamepad forwarding off, unless `--audio-device` / `--loopback-gamepads` say otherwise.
- Reviewed by the `unsafe-ffi-reviewer` and `latency-auditor` passes. Fixed:
  - the panel could fix the process DPI mode before the per-monitor-v2 call that desktop duplication needs;
  - clipboard memory leaked on failure paths;
  - the host kept sharing with no visible window if the panel failed to open;
  - closing the window killed the process mid-encode;
  - status rewritten every 10 ms while the server was unreachable;
  - synchronous log writes on hot threads;
  - launcher font leak and double-connect;
  - an inconsistent same-machine test.
- Not done yet: the launcher connects before the stream window's decoder exists, so the first IDR is usually missed and re-requested (~250 ms more to the first frame); the GUI client path ignores `--sim`.

## Input and cursor

- **Wire**: `Channel::Input` datagrams carry the newest event plus up to 7 previous ones (`proto::input`). The host applies each sequence number once, in order, so a lost datagram is repaired by the next one with no retransmit.
- **Mouse**: absolute by default. The window position maps through the letterbox to video coordinates, then to the captured monitor inside the host's virtual desktop.
- **Relative mode**: when the host cursor is hidden by an app (a game captured the mouse), the client switches to raw-input deltas and clips and hides its cursor.
  - Hidden-ness comes from `GetCursorInfo`, not DXGI: DXGI reports "invisible" whenever the pointer is on another monitor.
  - **Ctrl+Alt** releases a captured mouse, and clicking the window recaptures it.
- **Keyboard**: set-1 scancodes with the extended bit are injected with `KEYEVENTF_SCANCODE`, so the host keyboard layout applies. Alt/F10 don't open the client's window menu.
- **Focus loss**: the client sends `ReleaseAll`, and the host releases keys and buttons and **neutralizes virtual pads**. The host also does this when a client leaves, reconnects, or goes silent for 3 s (crash), so there are no stuck keys or sticks.
- **Relative-mode coalescing**: each `WM_INPUT` drains the queue with `GetRawInputBuffer` and sends one summed `MouseRel`, so a 4–8 kHz mouse doesn't cost one datagram and one `SendInput` per report.
- **Sending**: the input datagram is built under a lock and sent after releasing it, so the UI and gamepad threads never wait on each other's syscalls.
- **Cursor off the critical path**: the host reads a new pointer shape and sends cursor messages *after* the frame is submitted to NVENC (the frame is held until then). It sends cursor state only when visibility or shape changes. The client builds native cursors on the UI thread, after the frame is presented.
- `SendInput` failures (UIPI: elevated foreground window, secure desktop) are logged once per burst. Controlling those needs an elevated host.
- **Cursor**: DXGI pointer shapes (color, monochrome, masked color) are sent on `Channel::CursorShape`, fragmented like video. The client rebuilds them as **native hardware cursors** with `CreateIconIndirect`, so the cursor has zero network latency and inverting cursors (the I-beam) keep inverting. Cursor state (visible, shape id, position) is a single datagram on `Channel::Cursor`. A missing shape is re-requested with `Control::RequestCursorShape`.
- `client --view-only` sends nothing (benchmarks on one machine, where host and client share one mouse).

## Gamepad

- **Client**: XInput polled at 1 kHz. State is sent on change plus a full snapshot every 50 ms. DualShock/DualSense via `Windows.Gaming.Input` is not implemented yet (they work through Steam Input / DS4Windows as XInput).
- **Host**: ViGEmBus virtual Xbox 360 controllers (`vigem-client`), up to 4, plugged on first state and unplugged when the client leaves. Reports go straight from `net-rx` (a ~10–20 µs IOCTL, no thread hop). Only one report per pad per datagram: the newest *new* state, never the redundant history, which would briefly rewind the pad. Plugging runs on `gamepad-plug`, a failed pad waits 2 s before re-plugging, and early report errors after a plug are tolerated for 10 s while PnP finishes.
- **Rumble**: ViGEm notifications on the host become `Control::Rumble`, sent to the peer that is current at that moment, and the client applies them with `XInputSetState`.
- Requires the ViGEmBus driver on the host (installed: v1.22.0, `D:\dev\ViGEmBus`).
- When the host is on the same machine, gamepad forwarding is off by default (`--loopback-gamepads` forces it): the client would see the host's own virtual pads and feed them back.

## Audio

WASAPI loopback capture → Opus, 5 ms frames in restricted low-delay mode → small adaptive jitter buffer on the client → WASAPI shared low-latency render. Crates: `audio` (WASAPI, jitter buffer, resampler), `opus-sys` (vendored libopus built with `cc`), `proto::audio` (payload).

### Host (`audio::capture`, `host::audio_send`)
- Loopback-captures one playback device (`--audio-device`, default device otherwise; `host --list-audio` shows each device's engine periods).
- The audio engine runs a device at the smallest period any client asks for, and loopback data arrives once per period. So the capture thread first opens a **silent IAudioClient3 render stream at the device's minimum period**, which lowers capture latency for everything on that device. The same stream plays the `--test-tone`. The thread wakes on that stream's event, tops it up and drains loopback.
- Packets are stamped with their QPC position, converted to stereo 48 kHz (linear resampler if needed) and cut into 5 ms frames.
- Opus `RESTRICTED_LOWDELAY` (CELT only, 2.5 ms look-ahead), 128 kbps VBR. Encode 41 µs, decode 13 µs per frame (p50).
- One datagram per frame, carrying the frame **and copies of the frames 1, 2 and 4 back** (~4 × 80 B at 128 kbps). Loopback delivers a whole engine period at once, so consecutive datagrams leave back to back and one loss burst takes them together; spreading the copies over 20 ms leaves a copy of each frame outside a burst of up to 3. Header `capture_us` = capture time of the frame's first sample.
- Loopback is event-driven where supported (it wakes when the engine has written the packet), otherwise polled on the keeper stream's event. The thread reopens the device on loss (unplugged, default changed, exclusive mode elsewhere).
- Timestamps: WASAPI stamps a loopback packet with the QPC time its first sample reaches the host's own output. Latency measured against it is therefore what remote playback adds over listening locally.

### Client (`audio::render`, `audio::jitter`)
- The stream thread stamps each audio datagram at `recv_from` (so its own video work doesn't read as network jitter) and pushes it into a wait-free SPSC ring (`rtrb`). The render thread (MMCSS "Pro Audio") runs event-driven at the device's minimum period and keeps one period + 0.5 ms queued in the device. If a wake-up ever finds the device dry, it switches to two periods for the rest of the session. It reopens the device on loss.
- Jitter buffer indexed by frame number:
  - Target depth = 98th percentile of transit jitter over the last 2–4 s, plus 1.5 ms, rounded up to frames. The worst 2% is ignored, so one receive stall doesn't hold extra delay for seconds; repeated stalls raise the target.
  - The first second of a stream isn't measured (the receiver's own start-up stall), and starts at 2 frames.
  - A missing frame with later ones present → Opus PLC. Empty → silence, then re-prime.
  - Nothing buffered, but a frame arrived within the last target + 1 pops (the newest frames were lost) → PLC and keep going; only a stream gone quiet is an underrun.
  - Below target → one stretch (PLC) per 100 ms at most.
  - Minimum depth over 200 ms above target → drop one frame (removes slack left by a stretch, clock drift).
  - Above target + 4 for 100 ms → drop straight back to target + 1 (one short glitch instead of seconds of extra delay).
- Latency is measured per frame: the time its first sample leaves the device queue (now + queued padding) minus host capture time. That's exact on one machine (shared QPC).
- On a one-machine test the client must play on a device the host isn't capturing (`--audio-device`), or the host hears its own stream back.

## Measurement

- **Link simulator** (`net::sim`, `--sim` on host and client): Gilbert-Elliott bursty loss, delay plus jitter, and a bandwidth bottleneck with a drop-tail queue, applied to everything a socket sends. It's reproducible, needs no driver (replaces the clumsy plan), and matched its settings in tests (10% loss → 10.5%, 15 ms → 15.3 ms, 10 Mbps / 20 ms queue → exactly 25 of 100 packets).
- **`host --test-pattern`**: scrolling structured texture with grain, a worst case like fast game motion that NVENC can still compress to its target. Pure random noise can't go below ~38 KB/frame at any quantizer, which makes congestion tests meaningless.

- Every stage records microsecond timestamps. The client HUD shows capture, encode, network, decode and present times, plus the total.
- **`bench photon`** (click-to-photon, app-agnostic): a borderless square on the captured monitor toggles black/white on every click it receives (`WS_EX_NOACTIVATE`, so the client keeps focus). The bench clicks the square's image inside the client window (move + down + up in one `SendInput`), then reads a 4x4 block of the client's monitor with desktop duplication until the toggle appears. The time runs from `SendInput` to that frame's `LastPresentTime`: input forwarding, injection, the square's repaint, DWM, capture, encode, network, decode, present and the client-side composition. Scanout is not included. `--direct` clicks the square itself: the same measurement with no streaming, the local baseline. `--class` points it at another app's stream window.
  - One-machine caveats: the cursor must start on the captured monitor (the bench parks it there), or the host reports a hidden cursor and the client switches to relative mouse mode. Desktop duplication of the client's monitor probably keeps DWM composing, so the gain from fullscreen independent flip only shows partly.
- Network conditions are simulated with **clumsy** (loss, jitter, throttling).

## Roadmap

| Phase | Deliverable | Gate |
|---|---|---|
| 0 | Toolchain + workspace | `cargo build` passes |
| 1 | Capture + NVENC → `.h264` file | encode < 3 ms @1080p |
| 2 | NVDEC client playing a file | decode < 2 ms |
| 3 | Loopback sub-frame streaming + HUD | < 8 ms p95 |
| 4 | Input, local cursor, gamepad + rumble | press-to-inject < 1 ms |
| 5 | FEC, NACK, ref invalidation, congestion control, link simulator | smooth at 5% loss ✅ (118–120 fps, ≤ 9 ms recovery) |
| 6 | Signal server, ID connect + approve, encryption, hole punch, relay | ✅ on loopback (direct + relay); across real NATs not yet tested |
| 7 | Audio | < 20 ms audio latency ✅ (8–11 ms p50 / 8–11 ms p95 on loopback) |
| 8 | 8a adaptive resolution ✅; HEVC/AV1, 4:4:4, virtual display, service mode, multi-region relays | 2 Mbps link streams (8a ✅: 54–59 fps, 0 lost) |

Each phase ends with a local git commit.

## Measured results

RTX 5060, driver 617.14 (NVENC API 13.1), 1920x1080 desktop, H.264 High, CBR 30 Mbps, 4 slices, 120 fps.

| Phase | Metric | p50 | p95 |
|---|---|---|---|
| 1 | DXGI acquire → copy submitted → release | 0.13 ms | 0.25 ms |
| 1 | Encode, P-frame (submit → last slice) | 1.20 ms | 1.35 ms |
| 1 | Encode, IDR every frame | 1.33 ms | 1.45 ms |
| 1 | First slice available, IDR, **sub-frame on** | **0.27 ms** | 0.37 ms |
| 1 | First slice available, IDR, sub-frame off | 1.46 ms | 1.70 ms |

| 2 | NVDEC decode: AU in → NV12 written into D3D11 textures (paced 120 fps) | 1.06 ms | 1.53 ms |
| 2 | NVDEC decode, unpaced, IDR every frame | 0.79 ms | 1.05 ms |
| 2 | Shader convert + `Present(0, ALLOW_TEARING)` call | 0.10 ms | 0.14 ms |
| 2 | Output vs FFmpeg reference decode (PSNR) | 74–77 dB, max error 1/255 | |

Color: NVENC converts ARGB input as **BT.709 limited range** (measured with `host --color-probe`), and the encoder now signals that in the VUI. The client shader inverts it and samples chroma left-sited per the H.264 default.
GPU clock note: paced decode is slower than unpaced (1.06 vs 0.75 ms p50) because the GPU downclocks between frames. Pinning clocks is a later optimization.

| 3 | **Loopback end-to-end: DWM present on host → `Present` returned on client** (1080p120, IDR every frame) | **2.6–3.0 ms** | **3.3–4.2 ms** |
| 3 | Same, real changing desktop content at ~565 fps (client window mirrored on the captured monitor) | 2.90 ms | 3.81 ms |
| 3 | Stage split: host (present → last datagram sent) / network / decode / present | 1.6–1.9 / 0.02 / 1.0 / 0.09 ms | |
| 3 | Network stage, sub-frame on vs off (31 KB frames) | 0.02 ms vs 0.14 ms | |

| 4 | Keyboard/mouse: client send → batched `SendInput` done on host | 0.072–0.090 ms | ≤0.26 ms |
| 4 | Gamepad: client send → value visible in XInput on the virtual pad (`bench gamepad`) | 0.090–0.105 ms | 0.175–0.188 ms |
| 4 | Rumble: game `XInputSetState` → host forwards to client | 0.131–0.146 ms | 0.17–0.23 ms |
| 4 | Absolute mouse mapping: video coordinate → host pixel | exact (12/12 live; unit test over ~9,400 points on both monitors) | |
| 4 | Video end-to-end after Phase 4 (regression check, IDR every frame) | 3.04 ms | 3.59 ms |
| 4 | Silent client (hard kill) → host drops it and releases input | 3.0 s | |

Phase 4 notes:
- Win/Alt+Tab and other system shortcuts are not captured yet (that needs a low-level keyboard hook).
- Injecting into elevated windows or the UAC secure desktop needs an elevated host or service (Phase 8).
- The client's XInput path was verified with virtual pads only, because no physical controller is attached.
- Relative `MOUSEEVENTF_MOVE` goes through the host's pointer speed and acceleration. Games reading raw input are unaffected, but desktop cursor speed in relative mode is non-linear.
- The host socket is deliberately **blocking**. Clones share one OS socket, so non-blocking mode would also make `net-rx` spin. A send can only block once the 4 MB send buffer is full, and congestion control (Phase 5) is what keeps it from filling.
- Before this commit, the project's `unsafe-ffi-reviewer` and `latency-auditor` agents reviewed Phase 4, and their findings were fixed: a host crash on a pointer-shape `ACCESS_LOST`, stuck virtual pads, ViGEm plugging blocking input, rumble misrouting, a GDI leak, a stale clip rect, and cursor work on the video path.

| 5 | Loopback, test pattern, no impairment | 3.3–3.6 ms | 4.7–5.3 ms |
| 5 | 1% random loss: frames lost / fixed by FEC | 0 lost (225 chunks repaired) | 3.46 ms p50 |
| 5 | 5% loss, bursts of 2: fps / lost frames / recovery | 118–120 fps, 5–7 lost per 12 s, ≤ 9 ms | 3.5 ms p50 |
| 5 | 10% loss, bursts of 3 | 114 fps, 31–37 lost per 12 s, recovery ≤ 8 ms | 3.5 ms p50 |
| 5 | NACK effect at 5% bursty loss (lost frames per 12 s) | 94 → 4 | |
| 5 | 20 Mbps bottleneck, 50 ms queue (encoder wants ~30) | 120 fps at 15–17 Mbps, 0 loss | 10.4 ms p50 |
| 5 | 10 Mbps, 15 ms each way, 1% loss | 30 fps (adaptive), 0 lost | 28 ms p50 |
| 5 | 30 ms RTT, 5% bursty loss (NACK off by design) | 78–85 fps, recovery ≤ 82 ms | 20 ms p50 |

Phase 5 notes:
- Every frame is fully protected; the remaining cost under loss is hidden frames while a recovery travels one RTT. At 30 ms RTT that's ~4–8 frames per unrecoverable loss.
- The 10 Mbps / 30 ms case settles conservatively at ~2.5–4 Mbps and 30 fps. It's stable with zero loss, but leaves headroom unused.
- Not done yet, from the latency audit:
  - encode a recovery frame immediately on a loss report instead of at the next capture slot;
  - pool the reassembler's per-chunk buffers;
  - move NACK resends off `net-rx`, so input never waits behind them;
  - send pacing within a frame.
  - The simulator's thread also never exits (tests and benchmarks only).
- An earlier batch of runs showed 20–40 ms GPU stalls and ~300 ms freezes. They came from a game running on the same GPU; on an idle machine they're gone.

| 6 | AES-GCM seal / open, 1200-byte datagram (ring backend; RustCrypto `aes-gcm` was 0.9 / 1.0 µs) | 0.3 / 0.3–0.4 µs | 0.4 / 0.6 µs |
| 6 | Connect time on loopback: CONNECT sent → authenticated, sealed stream confirmed (auto-approve) | 1–6 ms | |
| 6 | Loopback, test pattern, **encrypted**, direct path | 3.28 ms | 3.82 ms |
| 6 | Same, IDR every frame | 3.63 ms | 4.30 ms |
| 6 | Encrypted, **relayed** through the signal server (net stage 0.13 vs 0.06 ms) | 3.45 ms | 4.17 ms |
| 6 | Relayed, IDR every frame (net stage 0.16 / 0.33 ms) | 3.67 ms | 4.72 ms |
| 6 | **Internet**, relayed via VPS (host → VPS → client on the same PC: crosses the home uplink and downlink once each; ping RTT to VPS 63 ms) | net 72 ms, TOTAL 75.4 ms | TOTAL 84–91 ms |
| 6 | Same: congestion control result on a 3.7 Mbps home uplink (measured with scp; downlink 12 Mbps) | 2.0–2.8 Mbps, 30 fps, 0 lost, RTT 132 ms | |
| 6 | Same, rate forced to 8 / 20 Mbps (CC floor raised) | 4.1 Mbps received, 40–77% loss, RTT 810 ms | |

Phase 6 notes:
- Internet relay test: the relay adds ~0 ms beyond the path (net 72 ms ≈ one 63 ms RTT for the two legs + serialization at 2 Mbps). The home uplink (3.7 Mbps) is the hard cap for any session hosted on this PC. Forcing a higher rate shows why the delay-based controller is needed: bufferbloat drives RTT from 132 to 810 ms. The controller settles near its 2 Mbps floor rather than at ~3.5 Mbps because internet queuing jitter (3–7 ms) straddles the 5 ms cut threshold; tuning that is a follow-up.
- Cross-machine LAN run (Ubuntu laptop `probe`, connected by ID via the VPS): the hole punch picked the laptop's private LAN address directly; 120 fps, ~26 Mbps, RTT 1.7–5.6 ms over Wi-Fi, 0% loss. The same run with `--force-relay` crossed the ADSL uplink twice: RTT ~127 ms, 0 loss after startup.
- That relay run exposed a startup flood: the controller began at the 30 Mbps maximum and built a 350 ms queue with 42% loss on the ~2 Mbps uplink before the first cut. Fixed with a slow start: begin at 2.5 Mbps, ramp 25% per 100 ms until the first congestion signal (loopback still reaches 30 Mbps, 3.13 ms p50). The floor drops from 2 to 0.5 Mbps.
- Queuing delay is now taken from each frame's first datagram. On a slow link, a frame's later datagrams wait behind its earlier ones (10 KB takes 40 ms at 2 Mbps), and the old per-datagram minimum mistook that burst for a standing queue.
- Known limit at the time: links under ~4 Mbps. Simulated 2 Mbps / 60 ms RTT with the 1080p test pattern failed (0 fps): even at 30 fps the encoder's floor was ~4 KB per P-frame plus 25 KB IDRs, so queue overflow, recovery and keyframes fed each other. Fixed in Phase 8a by adaptive resolution.
- Signal server deploy: `deploy/signal` (Dockerfile + docker-compose, built from `deploy/signal/bundle.sh`). UDP 7000 is published directly, never through a reverse proxy, since the server must see real peer addresses.
- `probe` (headless receiver, builds on Linux) is for the real-NAT test from a second machine.
- Encryption costs nothing measurable end to end (Phase 5 unencrypted: 3.3–3.6 ms p50).
- The relay adds ~0.07 ms p50 on loopback: one extra hop, forwarded one datagram at a time. Real relay cost is dominated by the path through the server.
- Not tested yet: real NATs. That needs the signal server on a public host (a small VPS) and the client on another network (e.g. a phone hotspot).
- Not done yet, from the reviews:
  - relay forwarding with URO/USO batching and a high-priority thread;
  - IPv6 candidates (rendezvous carries IPv4 only);
  - only one LAN candidate (the default-route address) is advertised;
  - congestion control doesn't count the 25–30 bytes of seal/relay overhead (~2.5%).
- Reviewed by the `unsafe-ffi-reviewer` and `latency-auditor` passes. Fixed: a replay check-then-mark race, a recv loop that could starve `net-rx` housekeeping under junk traffic, whole-slice sealing before the first send, buffer re-zeroing, a spoofable popup name, unbounded popups, and relay socket buffers.

| 7 | **Audio capture → device output** (loopback, test tone, both devices at their 10 ms minimum engine period; session histogram over ~3,500 frames) | **8.0–10.7 ms** | **8.2–10.7 ms** (p99 10.8–13.1) |
| 7 | Same, 5% loss in bursts of 2 (video + audio on the same lossy link) | 12.8–15.8 ms | 17.8–20.8 ms |
| 7 | Frames concealed at 5% bursty loss: copies 1 back only → copies 1, 2, 4 back | 1.4% → 0.1% | |
| 7 | Opus encode / decode, 5 ms stereo frame | 41 / 13 µs | 69 / 25 µs |

Phase 7 notes:
- Every playback device on the dev box (NVIDIA HDMI, Realtek, a virtual 7.1 device) reports a 10 ms minimum engine period, so 10 ms is the floor here. Drivers that support smaller periods (e.g. Microsoft's in-box HD Audio driver, often ~2.7–3 ms) are used automatically, and should bring latency down by roughly 2 × (10 − their period) ms.
- The ~11 ms splits as: one engine period on the capture side (loopback delivers once per period), plus the playout queue (1–2 frames of jitter buffer, since loopback delivers 2 frames per period at once) and the device queue. The network adds ~0 on loopback.
- An early version locked onto start-up stalls (receiver decoder creation) and drained excess one frame per second: 80–135 ms. Warm-up, percentile targeting and fast drain fixed that.
- Reviewed by the `unsafe-ffi-reviewer` and `latency-auditor` passes. Fixed: event handles leaked on error paths and closed before their client (now RAII, dropped after the client), audio dying for the session on device loss (now reopened), short decodes replaying a stale tail, the host audio thread never joined, no fallback without IAudioClient3, redundancy that one burst could wipe out, slack that stayed after a stretch, head loss treated as an underrun, and arrival times stamped after video work.
- Not done yet, from the reviews:
  - receive on its own thread (a window drag or decoder stall on the stream thread still delays audio);
  - non-blocking logging (the once-a-second report runs on the MMCSS thread; a console in QuickEdit selection would block it);
  - audio isn't counted by congestion control (~140 kbps);
  - clock-drift resampling (drift is corrected by dropping a frame);
  - audio/video sync (audio plays as early as possible, typically ahead of video).
- Restricted-low-delay Opus is CELT only, so Opus in-band FEC (LBRR, a SILK feature) isn't available; the spread copies replace it.

| 8a | Adaptive resolution, 2 Mbps bottleneck, 30 ms each way (was 0 fps at fixed 1080p) | 54–59 fps @ 360p, 0 lost | 46 ms p50 |
| 8a | 4 Mbps, 30 ms each way (was 31 fps, 3 s freezes, 12 lost) | 60–77 fps @ 360–540p, 0 lost | 40–44 ms p50 |
| 8a | 10 Mbps, 30 ms each way (was 30 fps) | 120 fps @ 540–720p, 0 lost | 39 ms p50 |
| 8a | Clean loopback (regression check) | 120 fps @ 1080p, 1.15 s after connect | 3.39 ms p50 / 4.29 ms p95 |
| 8a | Client freeze per resolution switch (NVDEC destroy + create → reconfigure) | ~49 ms → none | |

| B | **Click → photon, local baseline** (`bench photon --direct`: click the square, see it composed; 180 Hz monitor) | **5.8 ms** | **8.4 ms** (p99 8.5) |
| B | **Click → photon through the stream** (loopback, client windowed, desktop capture on change) | **13.5–15.7 ms** | 23.0 ms |
| B | Same, `--force-fps` (GPU stays clocked up, like during a game) | 13.4 ms | 18.1 ms |
| B | Same, `--force-fps`, client `--fullscreen` | **12.7 ms** | **17.6 ms** (p99 19.5) |
| B | Click reaches the square through the stream (client forward + network + host `SendInput`); direct: 0.58 / 0.77 | 2.7–3.1 ms | 3.5–3.9 ms |
| B | Host `SendInput`: one absolute move across monitors / a relative move (isolated, 100–200 calls) | 0.72 / 0.05–0.09 ms avg | max 1.6 ms |

Benchmark notes (B):
- What streaming adds over the local baseline, p50 with the GPU clocked up: about 7 ms. That splits into ~2.1 ms on the input leg, ~2.9 ms capture → present (HUD `TOTAL`) and ~2 ms waiting for the client's composition on its own monitor. The square's repaint and the host's DWM composition (~5.2 ms at 180 Hz) are in both numbers.
- The input leg is mostly Windows: the network takes 30–75 µs, but `SendInput` of an absolute move that crosses monitors takes 0.3–0.7 ms per call, and the click is three events. Two machines don't cross monitors, and a move within one costs less.
- Desktop mode at idle is ~2 ms slower at p50 and 5 ms at p95: with only a few changed frames per second the GPU clocks down, and encode climbs from 1.3 to ~5 ms, decode to 5–7 ms p95. A game keeps the GPU busy. Holding clocks up while a session is open is a follow-up.
- Parsec can't connect a PC to itself, so the Parsec side of the comparison needs a second machine (Parsec's Linux client on the laptop, with a Linux port of the bench) or uses Parsec's own overlay timings. Not measured yet.
- Reproduce: `host --listen 127.0.0.1:9000 [--force-fps]`, `client --connect 127.0.0.1:9000 --pos 2020,100 [--fullscreen]`, then `bench photon [--samples 200]`. Baseline: `bench photon --direct`.

Phase 8a notes:
- The tests use the scrolling test pattern, which is hard to compress. Normal desktop content fits higher levels at the same rate.
- Reviewed by the `unsafe-ffi-reviewer` and `latency-auditor` passes. Fixed: the switch IDR not flagged as a keyframe (the client discarded it and asked for another), a refused reconfigure exiting the host, loss reports from before the reset, a failed NVDEC reconfigure leaving no output planes, a CUDA registration leaked on partial failure, the ladder and the fps divisor locking each other in (ladder sized for 60 fps while NVENC budgets for 120), and the start-up flood (the session used to open at 1080p on any link).
- Not done yet, from the reviews:
  - drive switches from NVENC's average QP and measured overshoot, not only the rate target (content-aware);
  - a larger VBV for the switch IDR (under the one-frame VBV it is blurry for a moment at low rates);
  - congestion control is not reset for a second client in the same host session, so its slow start and ladder start are skipped;
  - fewer slices at small sizes (4 slices of 360p are tiny);
  - GPU timestamp queries to measure the downscale itself (estimated 20–50 µs).

Phase 3 notes: these numbers are steady state. The first ~0.2 s after connecting has 20–60 ms stalls from decoder creation and warm-up (pre-warming is a TODO).
Without client→host rate control, a client that falls behind drains a backlog from its socket buffer instead of skipping to fresh data. That gets fixed with feedback and congestion control in Phase 5.
URO (receive coalescing) and send pacing are not implemented yet: receive is not a bottleneck on loopback, so they'll be revisited with real links in Phases 5–6.
The HUD is currently the window title plus a once-per-second log line, showing per-stage p50/p95/max.

Reproduce: `host --list`, then `host --output 0 --fps 120 --force-fps [--idr-every 1] [--no-subframe] --dump out.h264`; `client --input out.h264 --fps 120` (or `--fps 0` unpaced, `--verify N out.bgra` for a readback).
Streaming: `host --listen 127.0.0.1:9000 [--force-fps --idr-every 1]` and `client --connect 127.0.0.1:9000 --pos 2020,100` (put the client on a monitor that isn't being captured, or on the same one for the mirror stress test).
The stream is checked with `ffprobe -count_frames` (all frames decode, no errors).
Caveat: an idle desktop is easy content. High-motion numbers come with the Phase 3 bench.

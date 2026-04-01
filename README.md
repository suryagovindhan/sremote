# sRemote — Enterprise Rust/WebRTC Remote Assist

Three standalone executables — no runtime installation required.

```
broker.exe   – Session Broker (signaling, JWT auth, TURN injection)
daemon.exe   – Privileged Capture Agent (screen capture, H.264 encode, control)
console/     – Technician HTML console (open index.html in any modern browser)
```

---

## Architecture

```
[Technician Browser]  <──WSS──>  [broker.exe]  <──WSS──>  [daemon.exe]
        │                                                       │
        │◄─────────────── WebRTC (video + DataChannel) ─────────┘
        │                  (direct or via TURN relay)
```

---

## Quick Start

### 1. Prerequisites

- Rust toolchain: https://rustup.rs
- **openh264.dll** (replaces FFmpeg for H.264 encoding)
  - Download: http://ciscobinary.openh264.org/openh264-2.4.0-win64.dll.bz2
  - Extract with 7-Zip, rename to `openh264.dll`, place it in `target\release\`

### 2. Configure `.env`

Edit `.env` in this folder:

```
JWT_SECRET=<random 32+ char string>
TURN_URL=turn:your-coturn-server:3478
TURN_SECRET=<coturn static-auth-secret>
TURN_USER=sremote
BROKER_ADDR=0.0.0.0:5695
BROKER_WS_URL=ws://127.0.0.1:5695
AGENT_ROOM=room-001
AGENT_SUBJECT=daemon-node-01
CAPTURE_FPS=30
INITIAL_BITRATE_KBPS=2000
```

### 3. Build

```cmd
build.bat
```

Both EXEs land in `target\release\`.

### 4. Run — Standalone (development)

**Terminal 1 — Broker:**
```cmd
cd target\release
broker.exe
```

**Terminal 2 — Daemon (on host to capture):**
```cmd
cd target\release
daemon.exe
```

**Browser — Technician Console:**
Open `console\index.html` directly in Chrome/Edge.  Fill in:
- Broker URL: `ws://localhost:5695`
- Room ID: `room-001`
- JWT Token: generate one (see §Tokens below)

---

## Install daemon.exe as Windows Service

Running as a Windows Service lets the daemon interact with the **Secure Desktop** (UAC elevation dialogs), keeping mouse/keyboard control active.

Run the following as **Administrator**:

```cmd
sc create "sRemoteDaemon" ^
    binPath= "C:\full\path\to\daemon.exe --service" ^
    DisplayName= "sRemote Privileged Capture Daemon" ^
    description= "Captures screen and relays control via WebRTC" ^
    start= auto ^
    obj= LocalSystem

sc start  sRemoteDaemon
sc query  sRemoteDaemon
sc stop   sRemoteDaemon
sc delete sRemoteDaemon
```

> **Note:** Copy `.env` to the same directory as `daemon.exe` OR configure the
> environment variables in the service using `sc config` or Group Policy.

---

## JWT Token Generation (quick test)

Use any HS256 JWT tool or the snippet below with `jsonwebtoken` CLI:

```python
import jwt, time
payload = {"sub": "tech-001", "role": "technician", "room": "room-001",
           "exp": int(time.time()) + 86400}
print(jwt.encode(payload, "YOUR_JWT_SECRET", algorithm="HS256"))
```

---

## Coturn Setup (TURN server)

```
apt install coturn
# /etc/turnserver.conf
use-auth-secret
static-auth-secret=YOUR_TURN_SECRET
realm=your.domain.com
```

---

## Files

| File | Description |
|---|---|
| `broker/src/main.rs` | Session Broker — axum + JWT + room relay |
| `daemon/src/main.rs` | Capture Daemon — xcap + openh264 + webrtc-rs + enigo |
| `console/index.html` | Technician UI |
| `console/app.js` | WebRTC client + input scaling |
| `.env` | All secrets and config (never commit!) |
| `build.bat` | One-command build |

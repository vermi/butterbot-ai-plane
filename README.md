# ButterBot AI Plane

Let an AI drive, see, hear and talk through a [CircuitMess ButterBot](https://circuitmess.com/),
the little "you pass butter" robot. This repository holds two things:

- **Modified ButterBot firmware** with an *AI mode*. In AI mode the robot joins your Wi-Fi and
  streams its camera and microphone to your computer. It takes drive, speech and audio commands
  in return.
- **`bbplane`**, a small Rust program for your computer. It talks to the robot and exposes it as
  an **MCP server**, so Claude or any other MCP client can control it. It also serves a live view
  in the browser for humans.

The handheld controller, RC mode, the idle behaviours and the robot's own voice keep working. Some
stock features had to go to make room for Wi-Fi; see [what was removed](#design-notes).

## What an AI can do with it

| | |
|---|---|
| **Move** | `move`, `turn`, `drive`, `stop`. Each move is timed and stops by itself. |
| **See** | `look` (live camera frame), `snapshot`, `set_stream` |
| **Hear** | `listen` returns microphone audio as a WAV file, with loudness stats |
| **Talk** | `say` in the robot's own voice, or in a laptop TTS voice streamed to its speaker. `play_audio`, `set_volume` |
| **Sense** | `events`: front and bottom IR sensors, motion (shake, pick-up, fall), battery, button |
| **Recognize** | `remember_face`, `who_is_here`, `forget_face`. Enrolled people are greeted by name and reported as `face` events. |
| **Extras** | `led`, `run_scenario` (jokes, facts, time, add-on modules), `ir_list`, `ir_send`, `read_modules` |

## How it works

```
 Claude / any MCP client ──MCP (stdio or HTTP /mcp)──┐
 Browser ──live view: video, audio, drive pad────────┤
                                                     ▼
                                       bbplane (your computer)
                                         │  UDP :7531
                                         │  JPEG video, 16 kHz PCM both ways, JSON control
                                         ▼
 ButterBot ── AI mode: Wi-Fi + UDP link ─┘
     ▲
     └── Bluetooth LE: one-time provisioning and "wake into AI mode"
         (the handheld controller keeps using BLE as before)
```

**Entering AI mode.** Short-press the robot's power button, or let `bbplane` wake it over
Bluetooth; any control tool does this automatically.
1. The robot says "A I mode on", pauses its own behaviour, joins Wi-Fi and dials `bbplane`.
2. When `bbplane` answers, it starts the camera and microphone and the robot says "Connected".
   This takes a few seconds. Later joins are quicker, because the robot remembers the Wi-Fi
   channel and its address.
3. If it gets no answer within 30 s, it says "I can't reach the computer" and goes back to normal.

**Leaving AI mode.** Short-press the power button again, call `exit_ai_mode`, or stop `bbplane`.
The robot says "A I mode off". Without `bbplane` it gives up after 15 s ("Lost the computer").
Wi-Fi then turns off again.

## Repository layout

| Path | What |
|---|---|
| [`firmware/`](firmware) | ESP-IDF 5.5.3 firmware for the robot (ESP32-S3). It starts from CircuitMess's [open-source firmware](https://github.com/CircuitMess/ButterBot-Firmware-Public) v1.0.5. The [`stock-firmware`](../../tree/stock-firmware) branch holds that release unmodified, so `git diff stock-firmware HEAD -- firmware` shows every change. |
| [`plane/`](plane) | `bbplane`: robot link, MCP server, live view, BLE provisioning and a simulated robot (Rust workspace). See [`plane/README.md`](plane/README.md) for all options. |

## Quick start

You need:

- a ButterBot;
- a computer on the same network, with **2.4 GHz** Wi-Fi reachable by the robot;
- [Rust](https://rustup.rs) 1.91 or newer;
- [ESP-IDF 5.5.3](https://docs.espressif.com/projects/esp-idf/en/v5.5.3/esp32s3/get-started/), only for flashing.

### 1. Build the plane

```sh
cd plane
cargo build --release          # binary: plane/target/release/bbplane
cargo test                     # includes end-to-end tests against a simulated robot
```

### 2. Flash the robot

```sh
cd firmware
. ~/esp/esp-idf/export.sh
idf.py build
python tools/flash.py -p /dev/cu.usbserial-XXXX --all   # first time: everything (~4 min)
python tools/flash.py -p /dev/cu.usbserial-XXXX         # after that: just the app (~2 min)
```

When it finishes the robot switches itself off. Turn it on as usual by holding the button for a
second.

> **Why not plain `idf.py flash`?** The firmware switches the robot off whenever it boots without
> the power button held (the stock hold-to-turn-on check). esptool's own reset at the end of a
> flash also leaves the chip in its bootloader, with the light off and the button ignored.
> `tools/flash.py` flashes without extra resets and ends with a clean one. If the robot ever
> switches off during flashing, hold its button and run it again.

### 3. Tell the robot your Wi-Fi network

Turn the handheld controller off first; it holds the robot's Bluetooth link.

```sh
plane/target/release/bbplane provision --ssid "MyWiFi" --password "secret"
```

This stores the network and this computer's address on the robot. The password must be empty
(open network) or 8–63 characters.

> **macOS:** run Bluetooth commands (`provision`, `wake`) from Terminal.app or iTerm2. Terminals
> built into Warp, VS Code and other IDEs do not declare Bluetooth use, so macOS stops any
> program they start that touches Bluetooth; `bbplane` then says so instead of crashing.

### 4. Run it

```sh
plane/target/release/bbplane serve
```

- **Live view:** <http://127.0.0.1:7532/>. Video, listen live, hold-to-talk, drive pad
  (W A S D), face enrollment and an event log.
- **MCP over HTTP:** `http://127.0.0.1:7532/mcp`.

Connect Claude Code:

```sh
claude mcp add --transport http butterbot http://127.0.0.1:7532/mcp
```

Then ask Claude to look around, drive somewhere or say something. To try everything without a
robot, run `bbplane sim` in a second terminal.

### 5. Teach it your face (optional)

Put the robot in AI mode, sit about an arm's length in front of it, alone in view, and either
click **Remember me** in the live view or ask Claude to remember your face. From then on, in AI
mode, the robot says hello when it sees you (at most every 10 minutes), and Claude gets `face`
events and can ask `who_is_here`.

Recognition runs on your computer, not the robot. The first enrollment downloads two open models
from the OpenCV model zoo (about 40 MB, checked against pinned hashes). The robot's camera sits
low, so standing people are often only in view from the chest down. At 320×240 a face across the
room is only about 15 pixels wide, too small to recognize; closer is better.

## Safety

The safety rules copy what the stock firmware does in the same situation:

- **Driving from the live view** works like the handheld controller in RC mode: no checks. The
  robot drives while you hold a direction and stops when you let go.
- **Driving by the AI** gets the checks of the stock autonomous moves. The robot refuses to start
  a move when tipped over, and stops driving forward when its front sensor sees something.
  **On the robot this was tested with, the front sensor never reported anything** (standing,
  covered, or driving into an obstacle), so the obstacle stop did not work there. The base board
  does the sensing in its own firmware, which this project cannot change.
- **Every drive command expires on its own:** within 1 s for the live view, and at the end of the
  move's duration for AI moves.
- **Losing the link** stops the robot immediately. It leaves AI mode after 15 s without
  `bbplane`.

## Security

- **The live view and MCP endpoint have no login.** By default they listen only on
  `127.0.0.1`. Opening them to the LAN (`web_bind = "0.0.0.0"`) lets anyone on that network
  watch, listen and drive.
- **Other websites cannot reach them through your browser.** `bbplane` only answers requests
  addressed to an IP address, `localhost` or a name in `allowed_hosts`, and refuses browser
  requests from other origins. This blocks cross-site WebSockets and DNS rebinding.
- **The robot only accepts link traffic from the provisioned plane address.** The link is not
  encrypted, so use a network you trust.
- **Face data stays on your computer.** Enrollment stores a name and a few face embeddings
  (numbers, no images) in `faces.json` in the data directory, readable only by your user.
  `forget_face` deletes a person. Only enroll people who agree to it.

## Design notes

These are the lessons that shaped the firmware. Most of them came from the hardware.

- **Internal RAM is the real limit.**
  - The ESP32-S3 has 8 MB of PSRAM, but Wi-Fi, Bluetooth and DMA need *internal* RAM.
  - The stock firmware had about 37 KB of it free, and Wi-Fi needs about 32 KB.
  - The firmware also aborts on any failed allocation, and a reboot means the robot switches
    itself off.
  - So, like the stock RC mode, **Wi-Fi only runs in AI mode**, and it is started on the state
    machine thread exactly as RC mode does it.
- **Some stock features were removed to make room for Wi-Fi.** They are compiled out, so they
  are gone outside AI mode too:
  - on-robot voice commands (a short press of the power button now enters AI mode instead);
  - face and object detection: "who's this", remember/forget face, "what's this", and the idle
    "notice a person" and "observe" behaviours;
  - the phone link: notifications, music control and clock sync. Without clock sync the "current
    time" scenario cannot know the time;
  - dice, eight ball, IR learning/forgetting and dance, which needed speech or music input.

  The robot's own speech (flite), the other idle behaviours and scenarios, IR sending, add-on
  modules and RC mode remain. Free internal RAM went from about 27 KB to about 100 KB. Face
  recognition is back in AI mode, running on the computer instead (see step 5).
- **UDP, like the stock video feed.** TCP stalled and died on the robot's busy 2.4 GHz link
  (Bluetooth shares the radio); UDP just skips lost packets.
  - **Video** is split into datagrams of at most 1400 bytes and reassembled on the computer;
    incomplete frames are dropped.
  - **The robot has only two Wi-Fi TX buffers**, so `sendto()` fails as soon as two datagrams
    are in flight. The sender waits for a free buffer instead of dropping; dropping lost the
    tail of nearly every multi-datagram frame and looked like heavy radio loss. Real loss is
    well under 1%. `status` reports `video_dropped` and `wifi_rssi`.
  - **Control requests** are retransmitted until acknowledged, and the robot runs each request
    only once.
- **The camera streams 320×240 at about 5 fps, with a 640×480 `snapshot`.** At the board's
  default 27 MHz sensor clock, frames larger than the stock 128×128 lose lines in the camera DMA
  and come back corrupted. AI mode runs the sensor at 20 MHz, which gives clean frames.
  Frames are captured as RGB565 and JPEG-encoded on the CPU, the same path stock uses; that
  encoder limits the frame rate.
- **Bluetooth goes into the stock low-power mode during AI mode** (slow advertising), which
  leaves airtime for Wi-Fi.
- **The link protocol** is a single frame format: `magic | type | flags | len | payload`.
  - Rust side: [`plane/crates/bbproto`](plane/crates/bbproto).
  - C++ side: [`firmware/main/src/Link/LinkProto.h`](firmware/main/src/Link/LinkProto.h).

## Limitations

- **Camera:** about 5 fps live at 320×240, and slower at larger sizes. Sizes above 640×480 are
  experimental and may not fit in memory next to Wi-Fi. The sensor's own JPEG mode
  (`encoder = "sensor"`) stalls after a frame or two.
- **Network:** 2.4 GHz Wi-Fi only (an ESP32 limitation). Expect some lost audio and video on a
  busy network.
- **Microphone:** quiet, with no echo cancellation; it is muted while the robot speaks.
  `listen` returns audio, not text: transcribe it with your own speech-to-text if your client
  cannot handle audio.
- **Movement:** open loop (no wheel encoders), so `turn` estimates angles from time.
- **Removed scenarios:** those that needed the removed features (dice, eight ball, face
  detection, "what's this", phone controls, dance) are not available.

## Restoring the stock firmware

Download `ButterBot-v1.0.5.bin` from the
[CircuitMess releases](https://github.com/CircuitMess/ButterBot-Firmware-Public/releases). Then
flash it while holding the power button:

```sh
esptool -c esp32s3 -b 460800 -p /dev/cu.usbserial-XXXX write-flash 0 ButterBot-v1.0.5.bin
```

## Credits and license

- **Firmware:** based on CircuitMess's ButterBot firmware, Copyright (c) 2026 CircuitMess d.o.o.,
  MIT License (see [`firmware/LICENSE.md`](firmware/LICENSE.md)). ButterBot is a CircuitMess
  product; this project is not affiliated with CircuitMess.
- **Everything else** is MIT licensed; see [`LICENSE`](LICENSE).

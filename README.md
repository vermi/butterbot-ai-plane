# ButterBot AI Plane

Let an AI drive, see, hear and talk through a [CircuitMess ButterBot](https://circuitmess.com/),
the little "you pass butter" robot. This repository holds two things:

- **Modified ButterBot firmware** with an *AI mode*. In AI mode the robot joins your Wi-Fi and
  streams its camera and microphone to your computer. It takes drive, speech and audio commands
  in return.
- **`bbplane`**, a small Rust program for your computer. It talks to the robot and exposes it as
  an **MCP server**, so Claude or any other MCP client can control it. It also serves a live view
  in the browser for humans.

The stock features you use every day keep working: the handheld controller, RC mode, the idle
behaviours and the robot's own voice.

## What an AI can do with it

| | |
|---|---|
| **Move** | `move`, `turn`, `drive`, `stop`. Each move is timed and stops by itself. |
| **See** | `look` (live camera frame), `snapshot`, `set_stream` |
| **Hear** | `listen` returns microphone audio as a WAV file, with loudness stats |
| **Talk** | `say` in the robot's own voice, or in a laptop TTS voice streamed to its speaker. `play_audio`, `set_volume` |
| **Sense** | `events`: front and bottom IR sensors, motion (shake, pick-up, fall), battery, button |
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
Bluetooth; any control tool does this automatically. The robot pauses its own behaviour, joins
Wi-Fi and dials `bbplane`.

**Leaving AI mode.** Press the power button again, call `exit_ai_mode`, or stop `bbplane`; the
robot gives up after 15 s without it. Wi-Fi then turns off again, and the robot behaves exactly
like stock.

## Repository layout

| Path | What |
|---|---|
| [`firmware/`](firmware) | ESP-IDF 5.5.3 firmware for the robot (ESP32-S3). It starts from CircuitMess's [open-source firmware](https://github.com/CircuitMess/ButterBot-Firmware-Public) v1.0.1; the history shows every change on top of it. |
| [`plane/`](plane) | `bbplane`: robot link, MCP server, live view, BLE provisioning and a simulated robot (Rust workspace). See [`plane/README.md`](plane/README.md) for all options. |

## Quick start

You need:

- a ButterBot;
- a computer on the same network, with **2.4 GHz** Wi-Fi reachable by the robot;
- [Rust](https://rustup.rs);
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
idf.py -p /dev/cu.usbserial-XXXX flash        # first time: writes app and data partitions
```

> **Hold the power button while flashing.** The robot keeps itself powered only while its
> firmware runs. In the chip's bootloader nothing holds the power latch, so keep the button
> pressed for the whole flash (2–5 minutes). After the first full flash, `idf.py app-flash` only
> rewrites the app and is quicker.

### 3. Tell the robot your Wi-Fi network

Turn the handheld controller off first; it holds the robot's Bluetooth link.

```sh
plane/target/release/bbplane provision --ssid "MyWiFi" --password "secret"
```

This stores the network and this computer's address on the robot. The password must be empty
(open network) or 8–63 characters.

### 4. Run it

```sh
plane/target/release/bbplane serve
```

- **Live view:** <http://127.0.0.1:7532/>. Video, listen live, hold-to-talk, drive pad
  (W A S D) and an event log.
- **MCP over HTTP:** `http://127.0.0.1:7532/mcp`.

Connect Claude Code:

```sh
claude mcp add --transport http butterbot http://127.0.0.1:7532/mcp
```

Then ask Claude to look around, drive somewhere or say something. To try everything without a
robot, run `bbplane sim` in a second terminal.

## Safety

The safety rules copy what the stock firmware does in the same situation:

- **Driving from the live view** works like the handheld controller in RC mode: no checks. The
  robot drives while you hold a direction and stops when you let go.
- **Driving by the AI** gets the checks of the stock autonomous moves. The robot refuses to start
  a move when tipped over, and stops driving forward when its front sensor sees something.
- **Every drive command expires on its own:** within 1 s for the live view, and at the end of the
  move's duration for AI moves.
- **Losing the link** stops the robot immediately. It leaves AI mode after 15 s without
  `bbplane`.

## Design notes

These are the lessons that shaped the firmware. Most of them came from the hardware.

- **Internal RAM is the real limit.**
  - The ESP32-S3 has 8 MB of PSRAM, but Wi-Fi, Bluetooth and DMA need *internal* RAM.
  - The stock firmware had about 37 KB of it free, and Wi-Fi needs about 32 KB.
  - The firmware also aborts on any failed allocation, and a reboot means the robot switches
    itself off.
  - So, like the stock RC mode, **Wi-Fi only runs in AI mode**, and it is started on the state
    machine thread exactly as RC mode does it.
- **Some stock features were removed for AI mode's memory:** phone notifications and music
  control, on-robot voice commands, and face and object detection. The robot's own speech
  (flite), idle behaviours, scenarios, IR, add-on modules and RC mode remain. Free internal RAM
  went from about 27 KB to about 100 KB.
- **UDP, like the stock video feed.** The robot's 2.4 GHz link loses a lot of packets with
  Bluetooth sharing the radio. TCP stalled and died on that; UDP just skips lost packets.
  - **Video** is split into datagrams of at most 1400 bytes and reassembled on the computer;
    incomplete frames are dropped.
  - **Control requests** are retransmitted until acknowledged, and the robot runs each request
    only once.
- **The camera streams 128×128, like the stock RC feed.** Larger frames come back corrupted on
  this board while Wi-Fi is busy. Frames are captured as RGB565 and JPEG-encoded on the CPU, the
  same path stock uses.
- **Bluetooth goes into the stock low-power mode during AI mode** (slow advertising), which
  leaves airtime for Wi-Fi.
- **The link protocol** is a single frame format: `magic | type | flags | len | payload`.
  - Rust side: [`plane/crates/bbproto`](plane/crates/bbproto).
  - C++ side: [`firmware/main/src/Link/LinkProto.h`](firmware/main/src/Link/LinkProto.h).

## Limitations

- **Camera:** 128×128. Larger snapshot sizes are selectable but experimental.
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

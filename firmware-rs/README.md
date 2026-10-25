# ButterBot AI firmware (Rust)

The robot's side of [`bbplane`](../plane): an AI-only firmware for the ButterBot's ESP32-S3,
written in Rust on ESP-IDF 5.5 (`esp-idf-svc`). It replaces the C++ firmware in
[`../firmware`](../firmware) and speaks the same link protocol (`plane/crates/bbproto`, used here
directly), so the plane needs no changes.

It keeps the robot's bootloader and partition table, so it installs over Wi-Fi from the C
firmware (`bbplane ota`), and either firmware can install the other.

## Building

You need the esp Rust toolchain (`espup install --std -t esp32s3`), `ldproxy`
(`cargo install ldproxy`), and ESP-IDF 5.5.3 with its tools (or let `esp-idf-sys` download it).

```bash
firmware-rs/tools/build.sh
```

writes `target/xtensa-esp32s3-espidf/release/butterbot-ai.bin`. Install it with

```bash
plane/target/release/bbplane ota firmware-rs/target/xtensa-esp32s3-espidf/release/butterbot-ai.bin
```

## Layout

| File | What |
|---|---|
| `src/main.rs` | Boot: power-on check, update mode, devices, threads |
| `src/ai.rs` | AI mode: the plane's requests, drive safety, the status LED, telemetry |
| `src/link.rs` | The UDP link to the plane: hello, ping, request dedupe, send queues, video fragments |
| `src/ota.rs` | Update mode, probation and rollback |
| `src/camera.rs` | The OV3660 with its own JPEG encoder: live stream and snapshots |
| `src/audio.rs` | Microphone (PDM), speaker (I2S, jitter buffer), the robot's recorded phrases |
| `src/logger.rs` | Logs to the console and to the plane (also ESP-IDF's warnings and errors) |
| `src/net.rs` | Home Wi-Fi |
| `src/base.rs` | The motor/sensor base board (I2C 0x69) |
| `src/battery.rs`, `src/motion.rs` | Battery level; accelerometer pose and motion events |
| `src/power.rs`, `src/rtc.rs` | Power button and latch, crash guard, state kept across restarts |
| `src/led.rs`, `src/tca.rs`, `src/i2c.rs` | Status LED rhythms, the I/O expander, I2C |
| `src/store.rs` | Settings in NVS (the C firmware's layout) |

## The robot's voice

The robot speaks with the plane's voice: the plane makes it (flite's `kal16` voice, the stock
robot's, through the compressor and reverb the C firmware applied) and streams it to the speaker.
For when the plane is out of reach, a dozen status phrases ("A I mode on", "Lost the computer",
...) are recorded in flash (`assets/phrases.bin`, IMA ADPCM). To change them, edit
`tools/phrases.py` and `audio::Phrase` together and run `python3 firmware-rs/tools/phrases.py`
(needs `brew install flite`).

## Logs

The firmware's own messages (info and up) and ESP-IDF's warnings and errors go to the plane's log
as well (`bbplane.log`, target `robot`), so a serial cable is rarely needed. If it is,
`tools/serial_log.py` reads the console without resetting the robot.


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
| `src/net.rs` | Home Wi-Fi |
| `src/base.rs` | The motor/sensor base board (I2C 0x69) |
| `src/battery.rs`, `src/motion.rs` | Battery level; accelerometer pose and motion events |
| `src/power.rs`, `src/rtc.rs` | Power button and latch, crash guard, state kept across restarts |
| `src/led.rs`, `src/tca.rs`, `src/i2c.rs` | Status LED rhythms, the I/O expander, I2C |
| `src/store.rs` | Settings in NVS (the C firmware's layout) |

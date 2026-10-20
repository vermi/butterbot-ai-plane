# bbplane: ButterBot AI control plane

Runs on a laptop and lets an AI (over MCP) or a person (in the browser) drive a CircuitMess
ButterBot. It covers movement, live video, live audio, the speaker and text-to-speech. It needs
the modified robot firmware in [`../firmware`](../firmware). The handheld controller keeps
working when AI mode is off.

```
AI client --MCP (stdio or HTTP /mcp)--+
Browser  --live view, REST------------+--> bbplane --UDP :7531 (robot dials in)--> ButterBot
```

## Build

```sh
cargo build --release        # binary: target/release/bbplane (Rust 1.91 or newer)
cargo test                   # unit tests + end-to-end test against the simulated robot
```

## Flash the robot

Build the firmware with ESP-IDF 5.5.3:

```sh
cd ../firmware
. ~/esp/esp-idf/export.sh
idf.py build
idf.py -p /dev/cu.usbserial-XXXX flash
```

The robot powers itself off when its firmware is not running, which includes long flash
operations. Hold the power button for the whole flash.

To restore the stock firmware, flash `ButterBot-v1.0.5.bin` from the
[CircuitMess releases](https://github.com/CircuitMess/ButterBot-Firmware-Public/releases) while
holding the power button:

```sh
uvx esptool -c esp32s3 -b 460800 -p /dev/cu.usbserial-XXXX write-flash 0 ButterBot-v1.0.5.bin
```

## Provision (once)

Turn the handheld controller off, so it does not hold the robot's Bluetooth link. Power on the
robot, then run:

```sh
bbplane provision --ssid "MyWiFi" --password "secret"   # --host defaults to this laptop's LAN IP
```

The robot stores the Wi-Fi network and plane address. To change networks, run `provision` again.
The password must be empty (open network) or 8 to 63 characters. macOS asks for Bluetooth
permission the first time.

## How the robot connects

The robot has too little internal RAM to keep Wi-Fi up next to its own behaviours, so it only
runs Wi-Fi in AI mode, the same way the stock firmware only runs it in RC mode:

1. The plane wakes the robot over Bluetooth. Any control tool does this automatically when the
   robot is not connected, or you can run `bbplane wake`. The robot must be idle: not in RC
   mode, a voice command or a scenario.
2. The robot switches to AI mode (its autonomy pauses), joins Wi-Fi and dials the plane. This
   usually takes 5 to 15 s.
3. `exit_ai_mode`, a short press of the power button, or 15 s without the plane ends AI mode
   and turns Wi-Fi off again.

The robot uses Bluetooth for the wake, so keep it paired with this machine or in range, and keep
the handheld controller off while waking it.

## Run

```sh
bbplane serve     # live view at http://127.0.0.1:7532/, MCP at http://127.0.0.1:7532/mcp
```

The robot must be able to reach port 7531 on the laptop. On macOS, allow incoming connections
for `bbplane` when asked.

The web server only answers requests addressed to an IP address, `localhost`, or a name in
`allowed_hosts`, and refuses browser requests from other sites. This stops web pages you visit
from reaching the robot's camera, microphone or controls through your browser. The live view and
API have no login, so only set `web_bind = "0.0.0.0"` on a network you trust.

Connect an MCP client using either option:

- **HTTP**, while `bbplane serve` is running:
  `claude mcp add --transport http butterbot http://127.0.0.1:7532/mcp`
- **stdio**, where the client launches the plane itself (do not also run `serve`):
  `claude mcp add butterbot -- /path/to/bbplane mcp`

To try everything without hardware, run `bbplane sim` in a second terminal. It acts as a robot
that streams a test card, a 440 Hz tone and fake events.

## MCP tools

| Tool | What it does |
|---|---|
| `status` | Connection, mode, battery, sensors and stream state |
| `enter_ai_mode` / `exit_ai_mode` | Pause or resume the robot's own behavior. Control tools enter AI mode automatically. |
| `move`, `turn`, `drive`, `stop` | Timed movement. The robot stops by itself when the time runs out, the link drops, or a safety check trips. |
| `look` | Latest live camera frame (image) |
| `snapshot` | Still photo up to 2048x1536 |
| `set_stream` | Live camera resolution and frame rate |
| `listen` | Microphone clip as WAV, with loudness and speech-activity stats |
| `say` | Speak. Uses the robot's own voice by default; `engine: "command"` uses laptop TTS. |
| `play_audio`, `stop_audio`, `set_volume` | Speaker control |
| `events` | Long-poll robot events: proximity, motion, battery, button, scenario progress |
| `led`, `run_scenario`, `scenarios`, `ir_list`, `ir_send`, `read_modules` | Stock features and add-on modules |

**Resources:** `butterbot://camera/latest` and `butterbot://status`.

## Safety

- **Driving refusals:** the robot refuses to start a move when it is tipped over (accelerometer)
  and stops driving forward when something is in front; both can be switched off in `[safety]`.
  Driving while charging is allowed.
- **Drive timeouts:** each drive command lasts at most 5 s on the robot. Longer moves are
  refreshed by the plane.
- **Automatic stops:** if the plane disappears, the robot stops at once and leaves AI mode
  after 15 s.
- **Manual override:** a short press of the power button hands control back to the robot. A
  3 s hold shuts it down as usual.
- **First tests:** put the robot on a stand with its wheels off the ground.

## Configuration

`bbplane config` prints the effective settings and the file path (on macOS
`~/Library/Application Support/bbplane/config.toml`). All keys are optional:

```toml
web_bind = "127.0.0.1"   # use 0.0.0.0 to open the live view to the LAN
web_port = 7532
allowed_hosts = []       # extra host names for the web server, e.g. ["mylaptop.local"]
link_port = 7531
auto_enter_remote = true
ble_wake = true          # wake the robot over Bluetooth when it is not connected

[camera]
res = "qvga"             # r128, qvga, vga, svga, xga, hd, uxga, qxga
fps = 10                 # the robot reaches about 5-6 fps at qvga
quality = 12             # 4..63, lower is better
xclk_mhz = 20            # sensor clock; the board default (27) corrupts frames above 128x128

[mic]
gate = true              # mute the mic while the robot speaks (no echo cancellation)
gain = 1.0
history_secs = 30        # microphone audio kept for `listen` with past=true

[safety]
tilt = true              # refuse to start moving when tipped over
obstacle = true          # stop driving forward when the front sensor is covered

[tts]
engine = "robot"         # or "command" for laptop TTS streamed to the robot
command = []             # e.g. ["piper", "--model", "en_US-amy-medium.onnx", "--output_file", "{out}"]
text_on_stdin = false    # true for piper
```

The default `command` presets are `say` on macOS, `espeak-ng` on Linux, and SAPI on Windows.

## Protocol

The wire format is defined in `crates/bbproto`, and the firmware mirrors it in
`firmware/main/src/Link/LinkProto.h`. Every message is one framed UDP datagram: a JSON control
channel, JPEG video (split into fragments of at most 1400 bytes), and 16 kHz mono PCM audio in both
directions. Control requests are retransmitted until acknowledged.

# bbplane: ButterBot AI control plane

Runs on a laptop and lets an AI (over MCP) or a person (in the browser) drive a CircuitMess
ButterBot. It covers movement, live video, live audio, the speaker and text-to-speech, speech
recognition and face recognition, and can keep an agent attached so the robot runs itself. It
needs the modified robot firmware in [`../firmware`](../firmware). The handheld controller keeps
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

Build the firmware with ESP-IDF 5.5.3 and flash it with `tools/flash.py` (not `idf.py flash`;
the [main README](../README.md#2-flash-the-robot) explains why):

```sh
cd ../firmware
. ~/esp/esp-idf/export.sh
idf.py build
python tools/flash.py -p /dev/cu.usbserial-XXXX --all   # first time: everything
python tools/flash.py -p /dev/cu.usbserial-XXXX         # after that: just the app
```

The robot switches itself off at the end; turn it on as usual.

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

To have it start at login and restart if it crashes, run `bbplane service install` (a launchd
agent on macOS, a systemd user service on Linux; logs go to `<data_dir>/bbplane.log` on macOS).
Run it again after a rebuild to restart it on the new binary; `bbplane service status` and
`bbplane service uninstall` do what they say.

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
| `status` | Connection, mode, battery, sensors, stream state and free RAM (`ram_free`) |
| `enter_ai_mode` / `exit_ai_mode` | Pause or resume the robot's own behavior. Control tools enter AI mode automatically. |
| `move`, `turn`, `drive`, `stop` | Timed movement. The robot stops by itself when the time runs out, the link drops, or a safety check trips. |
| `look` | Latest live camera frame (640x480 by default) |
| `snapshot` | Still photo, 640x480 by default (larger sizes are experimental) |
| `set_stream` | Live camera resolution and frame rate |
| `listen` | Microphone clip as WAV, with loudness and speech-activity stats (and a transcript) |
| `hear` | Wait for the next command (speech addressed to the robot, transcribed here) or nudge |
| `reply` | Speak, then wait like `hear`: one call per conversational turn |
| `focus`, `unfocus` | Stay quiet (no nudges, no follow-up listening) until called by name or the time runs out |
| `end_conversation` | The person is done: listen for the name again |
| `power_off` | Switch the robot off, only when someone asked for it |
| `say` | Speak. Uses the robot's own voice by default; `engine: "command"` uses laptop TTS. |
| `play_audio`, `stop_audio`, `set_volume` | Speaker control |
| `events` | Long-poll robot events: proximity, motion, battery, button, scenario progress, faces |
| `remember_face`, `forget_face`, `who_is_here` | Face recognition, run on this machine (see below) |
| `remember`, `forget`, `memories` | Long-term memory across agent sessions |
| `remind`, `reminders` | Reminders and timers, delivered as nudges when due |
| `phone` | The paired phone: `status` (what is playing), `notifications`, `play`, `pause`, `next`, `previous` |
| `led`, `run_scenario`, `scenarios`, `ir_list`, `ir_send`, `read_modules` | Stock features and add-on modules |

**Resources:** `butterbot://camera/latest` and `butterbot://status`.

## Face recognition

`remember_face` (or **Remember me** in the live view) learns the one person in front of the camera
under a name, from 8 live frames. While the robot streams video in AI mode, about three frames a
second are checked:

- YuNet finds faces, and SFace turns each into a 128-number embedding. Both are run with
  [tract](https://github.com/sonos/tract), pure Rust with no native libraries.
- Each embedding is compared with the enrolled ones (cosine similarity, `threshold`).
- When someone has been seen in two checks in a row, a `face` event reports them as `arrived`
  (`name` is null for strangers). They are reported `left` after 6 s out of view.

`status` and `who_is_here` list the faces in view. The models are downloaded on first use into
`<data_dir>/models` and checked against pinned SHA-256 hashes. People are stored in
`<data_dir>/faces.json`: names and embeddings, no images, mode 0600.

## Object detection

With `[objects] enabled` (the default), about one frame a second goes through YOLOX-S (COCO's 80
everyday classes, 0.15 s a frame on an Apple Silicon CPU, with tract). The model (36 MB,
Apache-2.0) is downloaded into `<data_dir>/models` on first use and checked against a pinned
hash.

- **Noticing:** when one of the `notice` kinds (cat, dog, bird) has been in view for two frames,
  `hear` returns a `noticed` nudge with a picture, at most once per `notice_cooldown_secs`. The
  detector mixes animals up at this resolution, so they count as one kind, and the agent is told
  "an animal, maybe a cat" and checks the picture.
- **Company:** a person in view counts as company for idle nudges, even when the low camera
  sees no face.
- `status`, `who_is_here` and the live view (dashed blue boxes) show what else it detects. These
  are not passed to the built-in brains, which see the picture instead.

## Sound recognition

With `[sounds] enabled` (the default), the microphone stream goes through YAMNet (Google,
Apache-2.0; AudioSet's 521 classes) twice a second, the last 0.96 s each time, in about 3 ms.
The model is an ONNX conversion of the original (16 MB, weights unchanged), downloaded into
`<data_dir>/models` on first use and checked against a pinned hash. Windows whose loudest moment
stays below about -42 dBFS are skipped as room noise; louder ones are levelled first, since the
robot's microphone is quiet. Nothing is heard while the robot speaks (the mic is muted then).

When one of the `notice` sounds scores at least `threshold`, `hear` returns a `heard` nudge, at
most once per sound per `cooldown_secs`. Alarms (smoke, fire) are not in the list by default (people
hear those themselves); added, they must last about 1.5 s and come through focus mode. `status`
lists the most recent sounds of any kind, with scores, for tuning.

## Listening

With `[stt] enabled` (the default), the plane transcribes the robot's microphone while it is in AI
mode. Silero VAD finds where speech starts and ends, and Whisper (`base.en`, Metal on Apple
Silicon) transcribes it, typically in about 0.1 s. The models (about 150 MB) are downloaded into
`<data_dir>/models` on first use and checked against pinned SHA-256 hashes.

- **Only speech addressed to the robot becomes a command:** it starts with the wake word
  (`Butterbot`, matched loosely because Whisper spells it many ways), or starts within
  `follow_up_secs` (5 s) of the robot finishing an answer. Saying only the name opens an 8 s window for the
  command.
- **"Goodbye"** ends the conversation: the robot says bye and listens for its name again.
- Commands not picked up within `command_max_age_secs` are dropped.

## The agent

With `[agent] enabled = true`, `bbplane serve` keeps an agent attached while the robot is in AI
mode. It restarts the agent when it exits, stops it 20 s after the robot leaves AI mode, and starts
a fresh session every `session_minutes` at a quiet moment, with a recap of recent conversation.
**Pause** in the live view stops it.

- **`realtime`** (default): the built-in brain on OpenAI's Realtime API over one WebSocket.
  Commands go in as text, and the robot speaks the answer while it is still being written.
- **`text`**: the built-in brain on the Responses API with a text model. Cheaper.
- **`command`**: any agent that can use this plane's MCP server, started as a command. The
  default is Codex:
  `codex --search exec --skip-git-repo-check -c model_reasoning_effort="low" ... "{prompt}"`.
  `{prompt}` becomes the persona (see `src/persona.md`, or `[agent] persona`) plus the recap.

The built-in brains need an OpenAI API key, from `[brain] api_key_file` or `OPENAI_API_KEY`. They
have the robot's tools (including `body`, its battery, plug and floor state, and `phone`) plus web
search. Their instructions describe the body: tracks, a camera low at the front, the sensors, the
battery, and no hands. With `voice = "robot"` (the default) the model writes
text and the robot speaks each sentence in its own voice as soon as it is complete, which is
quick and costs no audio tokens. With an OpenAI voice the realtime model speaks itself and the
audio is streamed to the robot.

**Setting up Codex** (`kind = "command"`): add the plane to `~/.codex/config.toml`, and let Codex
call its tools without asking (nobody is at the keyboard to approve them):

```toml
[mcp_servers.butterbot]
url = "http://127.0.0.1:7532/mcp"
default_tools_approval_mode = "approve"
```

**Nudges.** `hear` and `reply` also return nudges, so the agent can act on its own:

- `call`: the paired phone is ringing (with the caller's name);
- `heard`: a sound worth noticing (see [Sound recognition](#sound-recognition));
- `reminder`: a reminder it set with `remind` is due (in focus mode too; with no agent to pass
  it on within a minute, the robot says it itself);
- `handled`: someone picked it up, put it down, shook it or dropped it;
- `battery`: the battery is at 20% or 10% and it is not plugged in, or it was just plugged in
  after such a warning;
- `arrived`: an enrolled person came into view after 30 minutes away;
- `noticed`: an animal came into view (see [Object detection](#object-detection));
- `idle`: nobody has talked to the robot for a random 2–5 minutes while someone is around (in
  view, or talking nearby), or 15–30 minutes while nobody is (at the normal activity level;
  quiet x6 with no idle nudges at all, calm x2.5, lively x0.4). It carries a suggestion weighted
  by the level: chat, news about `interests`, look around, `wander`, think out loud, or rest.

**Memory.** `remember` keeps a fact (at most 100, oldest dropped first) in
`<data_dir>/memory.json`; every new session's instructions list them, so the agent knows what it
learned before. `forget` drops one. The built-in brains have the same tools.

The agent decides what to do with each: greet, chat, look around, wander, or stay quiet. Commands
from `[life] owner` are marked `from_owner`, and the persona tells the agent to put them first.
In focus mode there are no nudges and no follow-up listening; a command that starts with the
robot's name ends it.

## Status light

The robot shows talking, joining Wi-Fi and losing the plane itself. The plane picks the other
states (`src/light.rs`) and sends them as `status_light` rhythms: a base level and a few flashes
per period, each level off, dim or bright. The `led` tool can still set the light; what it sets
stays until the state changes. The table is in the [main README](../README.md#status-light).

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
- **Tethered:** the agent does not drive while the robot is plugged in, unless
  `drive_when_plugged = true`. The live view's drive pad is not limited.
- **Power off:** `power_off` only works when the last command asked for it.
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
announce_connect = true  # the robot says "Connected." when it links up in AI mode

[camera]
res = "vga"              # r128, qvga, vga, svga, xga, hd, uxga, qxga
fps = 10                 # the robot reaches about 2 fps at vga, 5-6 at qvga
quality = 12             # 4..63, lower is better
xclk_mhz = 20            # sensor clock; the board default (27) corrupts frames above 128x128

[mic]
gate = true              # mute the mic while the robot speaks (no echo cancellation)
gain = 1.0
history_secs = 30        # microphone audio kept for `listen` with past=true

[safety]
tilt = true              # refuse to start moving when tipped over
obstacle = true          # stop driving forward when the front sensor is covered
drive_when_plugged = false  # let the agent drive while the robot is plugged in

[faces]
enabled = true           # recognize enrolled people in AI mode (idle until someone is enrolled)
greet = true             # say "Hi, <name>!" when an enrolled person arrives
greet_cooldown_secs = 600
welcome_back_minutes = 30  # someone counts as arriving only after being out of view this long
threshold = 0.40         # 0..1 similarity needed to name someone; higher = fewer mix-ups, more misses

[tts]
engine = "robot"         # or "command" for laptop TTS streamed to the robot
command = []             # e.g. ["piper", "--model", "en_US-amy-medium.onnx", "--output_file", "{out}"]
text_on_stdin = false    # true for piper

[objects]
enabled = true           # detect objects in AI mode (downloads a 36 MB model on first use)
notice = ["cat", "dog", "bird"]   # COCO classes whose arrival nudges the agent
threshold = 0.4          # detection confidence needed
notice_cooldown_secs = 600
pet_cooldown_secs = 1800 # with [life] pets set, a pet coming into view is remarked on at most this often

[sounds]
enabled = true           # recognise sounds in AI mode (downloads a 16 MB model on first use)
notice = ["Bark", "Doorbell", "Ding-dong", "Knock", "Beep, bleep", "Baby cry, infant cry",
          "Shatter", "Meow"]   # AudioSet names; alarms can be added (they come through focus mode)
threshold = 0.3
cooldown_secs = 300

[stt]
enabled = true           # transcribe the microphone in AI mode
wake_word = "Butterbot"  # empty: everything said is a command
command_max_age_secs = 60
follow_up_secs = 5       # after an answer, start talking within this long to carry on without the name

[life]
enabled = true           # nudges for the agent
owner = ""               # enrolled name whose requests come first
pets = []                # e.g. ["Biscuit, a beagle"]: an animal it sees is probably one of them
activity = "normal"      # quiet, calm, normal or lively; changed by voice and kept across restarts
interests = []           # e.g. ["astronomy", "cooking"]: idle chat brings up recent news in these
nudge_in_view_secs = [120, 300]    # random wait between idle nudges with someone around
nudge_alone_secs = [900, 1800]     # ...and with nobody around
default_focus_minutes = 60
session_minutes = 30     # fresh agent session after this long (0: never)

[agent]
enabled = false
kind = "realtime"        # realtime, text or command
command = ["codex", "--search", "exec", "...", "{prompt}"]   # for kind = "command"
# persona = "/path/to/persona.md"

[brain]                  # the built-in brains (kind realtime or text)
# api_key_file = "~/openai-key"   # default: OPENAI_API_KEY
realtime_model = "gpt-realtime-2.1"
voice = "robot"          # or an OpenAI voice: marin, cedar, alloy, ash, coral, echo, sage...
text_model = "gpt-5.6-terra"
text_reasoning = "none"
tts_model = "gpt-4o-mini-tts"     # text brain with an OpenAI voice
search_model = "gpt-5.6-terra"    # web_search
```

The default `command` presets are `say` on macOS, `espeak-ng` on Linux, and SAPI on Windows.

## Protocol

The wire format is defined in `crates/bbproto`, and the firmware mirrors it in
`firmware/main/src/Link/LinkProto.h`. Every message is one framed UDP datagram: a JSON control
channel, JPEG video (split into fragments of at most 1400 bytes), and 16 kHz mono PCM audio in both
directions. Control requests are retransmitted until acknowledged.

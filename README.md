# ButterBot AI Plane

Let an AI drive, see, hear and talk through a [CircuitMess ButterBot](https://circuitmess.com/),
the little "you pass butter" robot. This repository holds two things:

- **An AI-only firmware** for the robot, written in Rust ([`firmware-rs/`](firmware-rs)). The
  robot joins your Wi-Fi and streams its camera, microphone and accelerometer to your computer,
  and takes drive, audio and light commands in return.
- **`bbplane`**, a small Rust program for your computer. It talks to the robot and exposes it as
  an **MCP server**, so Claude or any other MCP client can control it. It also serves a live view
  in the browser for humans.

`bbplane` can also **let the robot run itself**: it keeps an agent attached while the robot is in AI
mode. The agent is either its built-in brain on OpenAI's APIs or any MCP agent you name, such as
Codex. You talk to it by name ("Butterbot, what's the weather?"), it answers out loud, and when
nobody is talking to it, it does things of its own accord. See [Let it run itself](#6-let-it-run-itself-optional).

The robot lives in AI mode: it starts in it, and goes back to it on its own when it ends without
being switched off. The handheld controller (Manual Override included), the phone link, the add-on
cards and the robot's own voice keep working; the voice is now made on your computer. The stock
robot's behaviours and routines of its own are gone; see [design notes](#design-notes). New
firmware goes over Wi-Fi (`bbplane ota`, about 20 s).

## What an AI can do with it

| | |
|---|---|
| **Move** | `move`, `turn`, `drive`, `stop`. Each move is timed and stops by itself. |
| **See** | `look` (live camera frame), `snapshot`, `set_stream` |
| **Hear** | `hear` waits for the next thing someone says to the robot (transcribed on your computer), or for a nudge to do something. `reply` speaks, then does the same. `listen` returns microphone audio and a transcript. |
| **Talk** | `say` in the robot's own voice (flite's `kal16`, as the stock robot spoke, made on your computer), or another TTS voice. `play_audio`, `set_volume` |
| **Sense** | `events`: front and bottom IR sensors, motion (shake, pick-up, bump, fall), battery, button, add-on cards |
| **Recognize** | `remember_face`, `who_is_here`, `forget_face`. Enrolled people are greeted by name and reported as `face` events. Animals coming into view (a cat, a dog, a bird) are noticed too. |
| **Behave** | `focus` (stay quiet for a while), `end_conversation`, `power_off` |
| **Phone** | `phone`: the paired phone's notifications, what's playing, and play, pause, next, previous |
| **Extras** | `led` (the status light, or the LED card), `read_modules` (the add-on card: temperature and humidity, CO2, gas, motion, or a distance sensor on the prototyping card) |

## How it works

```
 Claude / any MCP client ──MCP (stdio or HTTP /mcp)──┐
 Browser ──live view: video, audio, drive pad────────┤
                                                     ▼
                                       bbplane (your computer)
                                         │  UDP :7531
                                         │  JPEG video, 16 kHz PCM both ways, accelerometer,
                                         │  JSON control
                                         ▼
 ButterBot ── AI mode: Wi-Fi + UDP link ─┘
     ▲
     └── Bluetooth LE: the handheld controller, the phone, one-time provisioning and
         "wake into AI mode"
```

**Entering AI mode.** Short-press the robot's power button, or let `bbplane` wake it over
Bluetooth; any control tool does this automatically.
1. The robot says "A I mode on", joins Wi-Fi and dials `bbplane`.
2. When `bbplane` answers, it starts the camera and microphone and the robot says "Connected".
   This takes two or three seconds. The robot remembers the Wi-Fi channel, which makes it quicker.
3. If it gets no answer within 30 s, it says "I can't reach the computer", turns Wi-Fi off and
   tries again quietly every 30 s.

**Leaving AI mode.** Short-press the power button again, or call `exit_ai_mode`. The robot says
"A I mode off", turns Wi-Fi off and stays out of AI mode until the next short press. If `bbplane`
goes away it gives up after 15 s ("Lost the computer") and keeps trying quietly. A long press
(2 s) switches it off.

## Repository layout

| Path | What |
|---|---|
| [`firmware-rs/`](firmware-rs) | The robot's firmware (ESP32-S3), in Rust on ESP-IDF 5.5.3 (`esp-idf-svc`). See [`firmware-rs/README.md`](firmware-rs/README.md). It replaced a C++ firmware built on CircuitMess's [open-source firmware](https://github.com/CircuitMess/ButterBot-Firmware-Public) v1.0.5 (in the history up to commit `66dd645`; the [`stock-firmware`](../../tree/stock-firmware) branch holds that release unmodified). |
| [`controller/`](controller) | The handheld controller's firmware (from CircuitMess's ButterBotCtrl-Firmware, MIT), changed for AI mode. |
| [`plane/`](plane) | `bbplane`: robot link, MCP server, live view, BLE provisioning and a simulated robot (Rust workspace). See [`plane/README.md`](plane/README.md) for all options. |

## Quick start

You need:

- a ButterBot;
- a computer on the same network, with **2.4 GHz** Wi-Fi reachable by the robot;
- [Rust](https://rustup.rs) 1.91 or newer;
- for building the firmware: the esp Rust toolchain (`cargo install espup ldproxy`, then
  `espup install --std -t esp32s3`) and [ESP-IDF 5.5.3](https://docs.espressif.com/projects/esp-idf/en/v5.5.3/esp32s3/get-started/)
  (or let the build download it).

### 1. Build the plane

```sh
cd plane
cargo build --release          # binary: plane/target/release/bbplane
cargo test                     # includes end-to-end tests against a simulated robot
```

### 2. Install the firmware

```sh
firmware-rs/tools/build.sh      # image: firmware-rs/target/xtensa-esp32s3-espidf/release/butterbot-ai.bin
```

A robot on this project's firmware updates over Wi-Fi, while it is in AI mode and the plane runs:

```sh
plane/target/release/bbplane ota firmware-rs/target/xtensa-esp32s3-espidf/release/butterbot-ai.bin   # ~20 s
```

The robot restarts into an update mode that runs nothing but Wi-Fi and the download, installs
the image in its spare app slot, and restarts into AI mode on it. It keeps the new firmware only
once it has reached the plane again; if it cannot within three minutes, or crashes, it goes back
to the one it had. A long press switches it off during the update. `bbplane restart` restarts a
robot that seems stuck.

A stock robot needs one USB flash first:

```sh
. ~/esp/esp-idf/export.sh       # for esptool
python firmware-rs/tools/flash.py -p /dev/cu.usbserial-XXXX --all
```

When it finishes the robot switches itself off. Turn it on as usual by holding the button for a
second. It starts in AI mode.

> **Why not plain esptool?** The firmware switches the robot off whenever it boots without the
> power button held (the stock hold-to-turn-on check). esptool's own reset at the end of a flash
> also leaves the chip in its bootloader, with the light off and the button ignored.
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
low, so standing people are often only in view from the chest down. At the default 640×480 a
face across the room is only about 30 pixels wide, barely enough to recognize; closer is better.
Someone counts as arriving (a hello, or a nudge for the agent) only after 30 minutes out of view,
and not while they are talking with the robot.

### 6. Let it run itself (optional)

Turn on the agent in `bbplane`'s config file (`bbplane config` shows where it is):

```toml
[agent]
enabled = true            # kind = "realtime" by default

[brain]
api_key_file = "~/path/to/openai-key"   # or set OPENAI_API_KEY

[life]
owner = "Sam"             # your enrolled name: your requests come first
pets = ["Biscuit, a beagle"]  # optional: an animal it sees is probably one of them
interests = ["astronomy"]     # optional: idle chat brings up recent news in these
```

Then run `bbplane serve` (or `bbplane service install` to have it start at every login) and
short-press the robot's button. From then on:

- **Talk to it by name:** "Butterbot, turn around." If you answer within 5 s, you can carry on
  without the name. "Goodbye" ends the conversation; it goes back to waiting for its name.
- **It has a life of its own.** When it sees someone it knows or an animal, is picked up or
  shaken, or nobody has spoken to it for a while, the agent gets a nudge. It decides for itself
  whether to greet, chat, look around or stay quiet. It chats more when someone is around.
- **"Butterbot, I need to focus"** makes it stay quiet (an hour by default, or as long as you
  say) until you call it by name. **"Butterbot, switch off"** powers it down; "goodbye" alone
  never does.
- **Activity levels:** "Butterbot, be more lively" / "calm down" / "be quiet for a while". Quiet
  only answers when spoken to; calm chats now and then; normal; lively chats often, wanders about
  and thinks out loud to itself. The level is kept across restarts and shown in the live view
  (which can set it too). Focus mode overrides all of it.
- **It remembers.** Sessions start fresh every half hour, but what it decides is worth keeping
  ("Kiwi hates the vacuum") goes into a memory it reads at every start.
- **Reminders and timers:** "Butterbot, remind me in 20 minutes to take the pizza out." They
  survive a restart and come through focus mode.
- **It knows its battery:** it asks to be charged at 20% and 10%, and says thanks when plugged in.
- **Phone calls:** with your phone paired, it tells you who is calling.
- **It hears things:** a bark, the doorbell, a knock, beeping. Sound recognition (YAMNet) runs on
  your computer too.
- **The status light** shows what it is doing; see [Status light](#status-light).
- **The live view** shows the conversation, what the agent is doing and its token use, what it
  remembers and the reminders that are set (both can be removed there), and has Pause and Focus
  buttons.

Speech is transcribed on your computer with Whisper, and objects are detected there with YOLOX-S
and sounds with YAMNet (about 210 MB of models in all, downloaded once and checked against pinned
hashes). When several
known people are in view, the agent is told it cannot tell which of them is talking. The agent can be:

| `[agent] kind` | What | Speed |
|---|---|---|
| `realtime` (default) | Built-in brain on OpenAI's Realtime API | Answers in about 1 s |
| `text` | Built-in brain on a cheaper text model (`gpt-5.6-terra`) | A little slower |
| `command` | Any MCP agent as a command, by default `codex exec` | 3–5 s with Codex |

The built-in brains speak in the robot's own voice by default, sentence by sentence while the
rest is still being written. Set `[brain] voice` to an OpenAI voice (`marin`, `cedar`...) to use
that instead. Both can search the web. See [`plane/README.md`](plane/README.md#the-agent) for
setting up Codex and all options.

## The remote in AI mode

The handheld controller works in AI mode too:

| Button | In AI mode |
|---|---|
| **Manual Override** | Held until the bar fills: the stick drives the motors straight over Bluetooth, and the remote's screen shows the camera (the remote joins your Wi-Fi and the computer streams to it, as in the camera view). The robot stays in AI mode; the agent does not drive meanwhile. Press it again to leave. Needs the remote's firmware from [`controller/`](controller), flashed once over USB. |
| **Shut up** | While it talks, or is about to answer: it stops at once and drops the rest. While it is quiet: focus mode. (In stock mode it still mutes.) |
| **Poke** | It reacts, then listens for a reply without its name. |
| **Summon** | A short press: says "Yes?" and takes what you say next as a command, no name needed. Held until the bar fills: comes to you (turns to find a person with the plane's object detection and drives over). |
| **Joystick menu** | Every entry goes to the agent as a request (a joke, the time, "what's this", the add-on cards...): the robot runs no routines of its own any more. |

In Manual Override the joystick mixes forward and turning continuously, so every angle curves by
its own amount (stock snapped it to eight directions). There are no checks, as in the stock RC
mode: mind the cable. Entering AI mode with a phone linked, it says so.

The remote's firmware is in [`controller/`](controller) (from CircuitMess's
ButterBotCtrl-Firmware, MIT). Its screen in AI mode:

- **Top bar:** the robot's clock on the left, AI mode's state in the middle (`AI` listening
  for its name, `AI CHAT` in a conversation, `AI ...` thinking, `FOCUS 42` minutes left in focus
  mode, `AI -` no agent running), the mute icon (only while muted) and the signal on the right.
  The clock is set from the phone, or from the computer in AI mode (the robot keeps it after).
- **Home screen**, whenever nothing else is on it: Butter Bot's face (pixel art, see Mood below)
  with his mood, what he is doing, his pace and both batteries beside it; below, in AI mode, the
  next reminder, the latest thing he saw or heard (and how long ago) and the last thing he said,
  scrolling; at the bottom, the phone's music while it plays. Outside AI mode, button hints
  instead of the AI lines.
- **Top bar:** a lightning bolt while the robot is plugged in.
- **Home screen:** a speech bubble shows what he says as he says it, or what you just said to
  him; beside the face, what he is doing ("dancing!", "wandering"), and the phone's music. The
  remote's own battery shows only when its AAs run low.
- **Joystick on the home screen (AI mode):** up opens the robot's camera, left and right step the
  activity level, quietly. Bluetooth is too slow for video, so the camera view has the remote
  join your Wi-Fi next to its own access point (the robot hands over the Wi-Fi details over their
  encrypted link, kept in RAM only) and the laptop streams frames to it in the RC feed's format,
  about 5 a second, with who and what is in view underneath. Any button or push closes it.
- **Joystick menu:** in AI mode, AI entries come first: What do you see?, News, Weather, Go
  explore, Tell me a story, Focus 1 hour and the four activity levels (the current one is marked).
  After Summon's short press the screen shows the listening window until the agent has the
  command.

## Mood

Butter Bot has feelings that last, and they are the agent's own: content, happy, excited, curious,
playful, calm, sleepy, bored, sad, grumpy, surprised or worried, with a strength. The agent sets
them with `feel` whenever something moves it and gets its mood back with every event, so a
feeling carries over between moments and sessions (it is kept in `mood.json` and fades back to
content over an hour or so). What happens to him moves it too unless he already feels something
stronger: someone arriving cheers him up, being shaken makes him grumpy, a poke surprises him, an
hour of nothing alone bores him, the small hours make him sleepy, being told to stop talking
saddens him a little. His mood sets how soon his own ideas come (bored or curious sooner, sleepy or
sad later) on top of the activity level, colours what he says, and shows on the remote's face.
Focus mode still wins: no ideas of his own, whatever he feels.

## His own life

- **Body language:** `gesture` makes small moves of the tracks that say something (wiggle, nod,
  shake, bounce, twirl, shiver, creep closer, back off, sulk) or dances. When music plays nearby
  (YAMNet hears it, and a simple onset tracker finds the beat) or starts on the Apple TV, he gets
  a `music` nudge and may dance to it.
- **Plans:** `intend` keeps things he means to do, at a time or some day; timed ones come back as
  a `plan` nudge (not in focus mode or when quiet), some-day ones come with idle nudges. The
  remote shows the next one when there is no reminder.
- **Diary:** the day's moments are logged as they happen; in the evening (`[life] diary_at`,
  21:30) he gets a digest and writes the day up himself (`write_diary`), or the next morning if
  he was off. Entries are in `diary/` in the data folder and in the live view.
- **Recall:** his memories (including old ones that no longer fit in his instructions), diary
  entries and each day's log (who came by, what was said, his dreams) are embedded with OpenAI's
  `text-embedding-3-small` and kept in `recall.json` (owner-only). What someone says brings related
  things to mind, which come with the message; `recall` searches on request, and
  `GET /api/recall?q=...` shows what a phrase brings up.
- **The cable and naps:** plugging him in or unplugging him is a `power` nudge the moment it
  happens. On the charger, after two quiet minutes, he naps: only his name, a poke or an alarm get
  through (as in focus mode), the remote shows him sleepy, and the plane writes him the odd dream.
  Once a charge, his nap does upkeep: it merges memories that say the same thing, keeps what is
  worth knowing from the day, and updates the recall index. Unplugged, he hears how long he slept,
  his dreams and what he kept. `[life] nap_when_charging = false` turns naps off.
- **Where things are:** `locate` gives the direction and floor distance of what the detectors see
  (or of any spot in the picture) and how long to drive there, from the camera alone (no extra
  model, no wait): bearings from where things sit across the frame, distances from where they meet
  the floor. `approach` goes up to something in short legs, facing it again before each. The
  optics are in `optics.json`; `bbplane optics --height-cm 9 --at-cm 100` sets the camera tilt
  from a known thing on the floor the object detector knows, and `--speed` measures the driving
  speed.
- **Being picked up:** the bottom sensor flickers (someone walking past, a shiny floor), so a lift
  counts once the floor has been away for 0.7 s, and an accelerometer pick-up only with the floor
  gone. The plane logs each flicker it ignored.
- **Voices:** WeSpeaker's ResNet34 (via sherpa-onnx's ONNX export, checked by hash) turns speech
  addressed to him into a voiceprint, so commands carry `speaker` even off camera. Voiceprints are
  kept for people who ask (`remember_voice`) and grow by themselves from commands spoken while
  that person is the only face in view; they are stored in `voices.json` (owner-only).
- **Apple TV:** `apple_tv` (pyatv in a helper process in its own virtual environment) plays,
  pauses, skips, changes volume, powers on and off, launches apps and reads what is playing. Set
  it up and pair it (a PIN for each protocol) from the live view's Apple TV card.
- When he acts on his own he just acts, without announcing it, and when he speaks up he leads with
  the point. Ideas that need driving are left out while he is on a cable.

The live view (http://127.0.0.1:7532/) shows his face and mood beside the camera, the
conversation, his plans, diary, memory and reminders, the people he knows by face and voice, the
Apple TV, driving, and the agent's output.

## Status light

In AI mode the robot's status light shows what it is doing. The light sits on an I/O expander pin
that can only switch on and off, so states differ by blink rhythm. Releasing the pin to the
expander's weak pull-up gives a faint glow (the one you see while holding the button at power-on).
It is only visible in a dim room, so it is used as a background hint, never on its own.

| Light | Meaning |
|---|---|
| A short tick every second | Joining Wi-Fi |
| Faint glow, a short bright tick every 3 s | Listening for its name |
| Bright, with a short wink off every 2 s | In a conversation: no name needed |
| Quick flicker | Heard you, thinking |
| Steady bright | Talking |
| Faint glow, a brief tick every 10 s | Focus mode |
| Dark, a slow faint pulse every 6 s | Napping on the charger |
| Two quick ticks every 4 s | No agent running (paused, or not set up) |
| Three quick blinks, pause | Cannot reach the computer |
| A short tick every second, dark between | Out of AI mode (switched off with a short press) |
| Bright, then fast blinking | Updating its firmware |

## Safety

The safety rules copy what the stock firmware does in the same situation:

- **Driving from the live view** works like the handheld controller in RC mode: no checks. The
  robot drives while you hold a direction and stops when you let go.
- **Driving by the AI** gets the checks of the stock autonomous moves. The robot refuses to start
  a move when tipped over, and stops driving forward when its front sensor sees something.
  **The front sensor only sees 2 to 3 cm ahead** (a hand held in front of it, on the robot this
  was tested with), so it stops the robot at the last moment rather than avoiding things. The
  base board does the sensing in its own firmware, which this project cannot change.
- **Every drive command expires on its own:** within 1 s for the live view, and at the end of the
  move's duration for AI moves.
- **A bump stops an AI move:** the accelerometer streams to the computer, and a sharp jolt while
  driving (a collision the front sensor missed) stops the robot and ends the move (`bump`).
- **Manual Override** drives without checks, like the stock RC mode.
- **Losing the link** stops the robot immediately. It leaves AI mode after 15 s without
  `bbplane`.
- **The agent does not drive while the robot is plugged in**: it would roll over the cable or
  pull itself off a table. The plane refuses (`[safety] drive_when_plugged`, and while it does not
  know yet whether the robot is plugged in), and so does the robot itself. The live view's drive
  pad is not limited.
- **The agent can switch the robot off only when someone asked for that** in the last command
  ("switch off", "shut down", "power off"...). Models have taken a plain "goodbye" for it.

## Security

- **The live view and MCP endpoint have no login.** By default they listen only on
  `127.0.0.1`. Opening them to the LAN (`web_bind = "0.0.0.0"`) lets anyone on that network
  watch, listen and drive.
- **Other websites cannot reach them through your browser.** `bbplane` only answers requests
  addressed to an IP address, `localhost` or a name in `allowed_hosts`, and refuses browser
  requests from other origins. This blocks cross-site WebSockets and DNS rebinding.
- **The robot only accepts link traffic from the provisioned plane address.** The link is not
  encrypted, so use a network you trust.
- **Memories and reminders** are kept in `memory.json` and `reminders.json` in the data
  directory, readable only by your user; the live view lists them and removes any.
- **Phone notifications** reach the agent only when someone asks for them, and with the
  built-in brain they then go to OpenAI like anything else it is told.
- **Speech is transcribed, and objects detected, on your computer.** Only what follows the robot's name (or is said in
  a conversation with it) goes to the agent, as text. With the built-in brain that text, the
  replies and an occasional camera frame go to OpenAI. The API key is read from the file you
  name and is never logged.
- **Face data stays on your computer.** Enrollment stores a name and a few face embeddings
  (numbers, no images) in `faces.json` in the data directory, readable only by your user.
  `forget_face` deletes a person. Only enroll people who agree to it.

## Design notes

The firmware ([`firmware-rs/`](firmware-rs)) is a rewrite in Rust of the C++ firmware this
project started with, itself built on CircuitMess's. The lessons that shaped both:

- **Internal RAM is the real limit.** The ESP32-S3 has 8 MB of PSRAM, but Wi-Fi, Bluetooth and
  DMA need *internal* RAM. The C++ firmware, carrying the stock framework, voice and models, had
  15 KB of it free in AI mode and ran Wi-Fi on two buffers of each kind. The Rust firmware keeps
  about 80 KB free with Wi-Fi (full buffers), Bluetooth and the camera running.
- **AI-only.** The robot lives in AI mode. The stock behaviours (rambling, wandering, reactions),
  routines (jokes, dice, dance...), on-robot voice commands, face and object detection and the IR
  card are gone; the agent does those things now, on the computer. The robot's voice is made on
  the computer too (the same flite voice), so a dozen status phrases are recorded in flash for when
  the computer is out of reach.
- **The camera encodes JPEG itself.** The C++ firmware captured RGB565 and encoded on the CPU
  (about 2 fps at 640×480). The OV3660's own JPEG output runs at about 26 fps at 320×240,
  640×480 and 800×600, with frames DMA'd through internal buffers (straight into PSRAM, the
  driver misses most frames' end).
- **Updates over Wi-Fi with a way back.** An update restarts the robot into an update mode (Wi-Fi
  and the download only), writes the spare app slot, checks size and SHA-256, and restarts into it.
  The new firmware must reach the plane within three minutes or it restarts and the bootloader
  goes back to the old one. A 1.6 MB image installs in about 20 s.
- **UDP, like the stock video feed.** TCP stalled on the robot's busy 2.4 GHz link (Bluetooth
  shares the radio); UDP just skips lost packets. Video is split into datagrams of at most 1400
  bytes and reassembled on the computer; control requests are retransmitted until acknowledged
  and run once.
- **Logs come to the computer.** The firmware's messages, and ESP-IDF's warnings and errors, go
  to the plane's log (`bbplane.log`, target `robot`), so a serial cable is rarely needed.
- **The link protocol** is one frame format, `magic | type | flags | len | payload`, defined
  once in [`plane/crates/bbproto`](plane/crates/bbproto); the firmware uses that crate too.

## Limitations

- **Camera:** about 26 fps up to 800×600; larger sizes are for snapshots (up to 2048×1536, about a
  second). The plane asks for 10 fps at 640×480 by default (`[camera]` in its config).
- **Network:** 2.4 GHz Wi-Fi only (an ESP32 limitation). Expect some lost audio and video on a
  busy network.
- **Microphone:** quiet, with no echo cancellation; it is muted while the robot speaks, so you
  cannot interrupt it. The S3's microphone input has no hardware high-pass filter; the firmware
  removes the rumble and DC offset in software.
- **Movement:** open loop (no wheel encoders), so `turn` estimates angles from time.
- **Sound recognition** is not tuned for the robot's quiet microphone yet: in a first test it
  heard a knock (as "chopping") and a cough, but not a doorbell or barking played from a phone.
  `status` lists the sounds it recently heard, with scores.
- **Object detection** knows COCO's 80 everyday classes: a fireplace comes out as an oven, and a
  dog lying down can come out as a cat. At 320×240 it misses small or still animals entirely.
- **No routines on the robot:** the remote's menu entries go to the agent, so they need the
  computer and an agent running.
- **Phone:** music controls act on whatever app the phone is playing from (open Spotify once to
  control Spotify); the robot cannot pick the app. It cannot reply to messages: Apple's
  notification service only lets an accessory read and dismiss them.

## Restoring the stock firmware

Download `ButterBot-v1.0.5.bin` from the
[CircuitMess releases](https://github.com/CircuitMess/ButterBot-Firmware-Public/releases). Then
flash it while holding the power button:

```sh
esptool -c esp32s3 -b 460800 -p /dev/cu.usbserial-XXXX write-flash 0 ButterBot-v1.0.5.bin
```

## Credits and license

- **Firmware:** written for this project, with parts ported from CircuitMess's ButterBot
  firmware (the hardware drivers' details and motion detection, and the voice effects the plane
  applies), Copyright (c)
  2026 CircuitMess d.o.o., MIT License (see
  [`firmware-rs/LICENSE-CircuitMess.md`](firmware-rs/LICENSE-CircuitMess.md)). The remote's
  firmware in [`controller/`](controller) is CircuitMess's, changed for AI mode. ButterBot is a
  CircuitMess product; this project is not affiliated with CircuitMess.
- **Everything else** is MIT licensed; see [`LICENSE`](LICENSE).

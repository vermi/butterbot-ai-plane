# ButterBot AI Plane

Let an AI drive, see, hear and talk through a [CircuitMess ButterBot](https://circuitmess.com/),
the little "you pass butter" robot. This repository holds two things:

- **Modified ButterBot firmware** with an *AI mode*. In AI mode the robot joins your Wi-Fi and
  streams its camera and microphone to your computer. It takes drive, speech and audio commands
  in return.
- **`bbplane`**, a small Rust program for your computer. It talks to the robot and exposes it as
  an **MCP server**, so Claude or any other MCP client can control it. It also serves a live view
  in the browser for humans.

`bbplane` can also **let the robot run itself**: it keeps an agent attached while the robot is in AI
mode. The agent is either its built-in brain on OpenAI's APIs or any MCP agent you name, such as
Codex. You talk to it by name ("Butterbot, what's the weather?"), it answers out loud, and when
nobody is talking to it, it does things of its own accord. See [Let it run itself](#6-let-it-run-itself-optional).

The firmware is AI-only: the robot starts in AI mode, and goes back to it on its own when it ends
without being switched off. The handheld controller, RC mode, the phone link and the robot's own
voice keep working. The stock robot's behaviours of its own are gone; see
[what was removed](#design-notes). After the first USB flash, new firmware goes over Wi-Fi
(`bbplane ota`).

## What an AI can do with it

| | |
|---|---|
| **Move** | `move`, `turn`, `drive`, `stop`. Each move is timed and stops by itself. |
| **See** | `look` (live camera frame), `snapshot`, `set_stream` |
| **Hear** | `hear` waits for the next thing someone says to the robot (transcribed on your computer), or for a nudge to do something. `reply` speaks, then does the same. `listen` returns microphone audio and a transcript. |
| **Talk** | `say` in the robot's own voice, or in a laptop TTS voice streamed to its speaker. `play_audio`, `set_volume` |
| **Sense** | `events`: front and bottom IR sensors, motion (shake, pick-up, fall), battery, button |
| **Recognize** | `remember_face`, `who_is_here`, `forget_face`. Enrolled people are greeted by name and reported as `face` events. Animals coming into view (a cat, a dog, a bird) are noticed too. |
| **Behave** | `focus` (stay quiet for a while), `end_conversation`, `power_off` |
| **Phone** | `phone`: the paired phone's notifications, what's playing, and play, pause, next, previous |
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
second. It starts in AI mode.

After that first flash, update it over Wi-Fi instead, while it is in AI mode and the plane runs:

```sh
plane/target/release/bbplane ota firmware/build/ButterBot-Firmware.bin   # ~1.5 min
```

The robot restarts into an update mode that runs nothing but Wi-Fi and the download (AI mode
leaves too little RAM for it), installs the image in its spare app slot, and restarts into AI mode
on it. It keeps the new firmware only once it has reached the plane again; otherwise it goes back
to the one it had. On any failure it restarts into AI mode on its old firmware. A long press
switches it off during the update. `bbplane restart` restarts a robot that seems stuck.

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
| **Manual Override** | Stock RC mode, with video on the remote's screen. RC mode needs the remote's own Wi-Fi, so the robot leaves AI mode for it; when you leave RC mode it restarts itself straight back into AI mode (a few seconds; RC mode leaves too little unfragmented memory to start the home Wi-Fi again without a restart). |
| **Shut up** | While it talks, or is about to answer: it stops at once and drops the rest. While it is quiet: focus mode. (In stock mode it still mutes.) |
| **Poke** | It reacts, then listens for a reply without its name. |
| **Summon** | A short press: says "Yes?" and takes what you say next as a command, no name needed. Held until the bar fills: comes to you (turns to find a person with the plane's object detection and drives over). |
| **Joystick menu** | Entries the robot can run in AI mode run as in stock. Those that needed the removed on-robot models (dice, eight ball, "what's this", "who's this", dance, voice moves) and reading notifications go to the agent as requests. |

In RC mode the joystick mixes forward and turning continuously, so every angle curves by its own
amount (stock snapped it to eight directions). Entering AI mode with a phone linked, it says so.

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
| Two quick ticks every 4 s | No agent running (paused, or not set up) |
| Three quick blinks, pause | Cannot reach the computer |

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

These are the lessons that shaped the firmware. Most of them came from the hardware.

- **Internal RAM is the real limit.**
  - The ESP32-S3 has 8 MB of PSRAM, but Wi-Fi, Bluetooth and DMA need *internal* RAM.
  - The stock firmware had about 37 KB of it free, and Wi-Fi needs about 32 KB.
  - The firmware also aborts on any failed allocation, and a reboot means the robot switches
    itself off.
  - So, like the stock RC mode, **Wi-Fi only runs in AI mode**, and it is started on the state
    machine thread exactly as RC mode does it.
- **The firmware is AI-only.** Stock mode is gone: the robot starts in AI mode, tries it again
  quietly every minute while the plane is out of reach, and outside AI mode it waits without
  behaviours of its own (no rambling, wandering, poke or motion reactions). The flash went to two
  app slots for updates over Wi-Fi; the stock models' partitions (wake words, faces, objects) are
  gone. Restore the stock firmware below to get all of it back.
- **Some stock features were removed to make room for Wi-Fi.** They are compiled out, so they
  are gone outside AI mode too:
  - on-robot voice commands (a short press of the power button now enters AI mode instead);
  - face and object detection: "who's this", remember/forget face, "what's this", and the idle
    "notice a person" and "observe" behaviours;
  - dice, eight ball, IR learning/forgetting and dance, which needed speech or music input.

  The robot's own speech (flite), scenarios, IR sending, add-on modules and RC mode remain. Free internal RAM went from about 27 KB to about 100 KB. Face
  recognition is back in AI mode, running on the computer instead (see step 5).
- **The phone link is back** (it was removed at first too): notifications, music control and
  clock sync from an iPhone, or an Android phone with the CircuitMess app. In AI mode, with Wi-Fi
  and a phone connected, about 25 KB of internal RAM stays free (`status` reports `ram_free`).
  The agent gets notifications as data and sums them up; the stock "read my notifications"
  routine reads them all without asking whether to go on, since it cannot listen for the answer.
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
- **The camera streams 640×480 at about 2 fps** (320×240 manages 5–6). At the board's
  default 27 MHz sensor clock, frames larger than the stock 128×128 lose lines in the camera DMA
  and come back corrupted. AI mode runs the sensor at 20 MHz, which gives clean frames.
  Frames are captured as RGB565 and JPEG-encoded on the CPU, the same path stock uses; that
  encoder limits the frame rate.
- **Bluetooth slows down during AI mode** (advertising every 211 ms, connection events every
  60–90 ms), which leaves airtime for Wi-Fi. Stock's own low-power settings (1.3 s and about 1 s)
  were too slow for a phone to connect: iOS gave up while setting up the notification service.
  Video loss with a phone connected stays about 1%.
- **The link protocol** is a single frame format: `magic | type | flags | len | payload`.
  - Rust side: [`plane/crates/bbproto`](plane/crates/bbproto).
  - C++ side: [`firmware/main/src/Link/LinkProto.h`](firmware/main/src/Link/LinkProto.h).

## Limitations

- **Camera:** about 2 fps live at 640×480 and 5–6 at 320×240. Sizes above 640×480 are
  experimental and may not fit in memory next to Wi-Fi. The sensor's own JPEG mode
  (`encoder = "sensor"`) stalls after a frame or two.
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
- **Removed scenarios:** those that needed the removed features (dice, eight ball, face
  detection, "what's this", dance) are not available.
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

- **Firmware:** based on CircuitMess's ButterBot firmware, Copyright (c) 2026 CircuitMess d.o.o.,
  MIT License (see [`firmware/LICENSE.md`](firmware/LICENSE.md)). ButterBot is a CircuitMess
  product; this project is not affiliated with CircuitMess.
- **Everything else** is MIT licensed; see [`LICENSE`](LICENSE).

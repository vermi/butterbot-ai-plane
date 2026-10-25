#!/usr/bin/env python3
"""Record the robot's own status phrases (for when the plane is out of reach) with flite's kal16
voice, the stock robot's, and pack them as IMA ADPCM into assets/phrases.bin.

    python3 firmware-rs/tools/phrases.py      # needs flite (brew install flite)

Keep PHRASES in the order of `audio::Phrase`.
"""

import os
import struct
import subprocess
import sys
import tempfile
import wave

PHRASES = [
    "Hello",
    "A I mode on.",
    "A I mode off.",
    "I can't reach the computer.",
    "Lost the computer.",
    "A I mode could not start: I have no Wi-Fi settings.",
    "Goodbye",
    "Battery level critical. Shutting down.",
    "Manual override.",
    "Back to A I mode.",
    "Camera failure.",
    "Motor board failure.",
]

STEPS = [
    7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 19, 21, 23, 25, 28, 31, 34, 37, 41, 45, 50, 55, 60, 66,
    73, 80, 88, 97, 107, 118, 130, 143, 157, 173, 190, 209, 230, 253, 279, 307, 337, 371, 408, 449,
    494, 544, 598, 658, 724, 796, 876, 963, 1060, 1166, 1282, 1411, 1552, 1707, 1878, 2066, 2272,
    2499, 2749, 3024, 3327, 3660, 4026, 4428, 4871, 5358, 5894, 6484, 7132, 7845, 8630, 9493,
    10442, 11487, 12635, 13899, 15289, 16818, 18500, 20350, 22385, 24623, 27086, 29794, 32767,
]
INDEX = [-1, -1, -1, -1, 2, 4, 6, 8, -1, -1, -1, -1, 2, 4, 6, 8]


def synth(text):
    with tempfile.TemporaryDirectory() as d:
        out = os.path.join(d, "p.wav")
        subprocess.run(["flite", "-voice", "kal16", "-t", text, "-o", out], check=True)
        with wave.open(out) as w:
            assert w.getframerate() == 16000 and w.getnchannels() == 1 and w.getsampwidth() == 2
            raw = w.readframes(w.getnframes())
    return list(struct.unpack(f"<{len(raw) // 2}h", raw))


def trim(s):
    loud = [i for i, x in enumerate(s) if abs(x) > 300]
    if loud:
        s = s[max(0, loud[0] - 400):loud[-1] + 1600]
    return s


def voice_fx(s):
    """The robot's speech chain (as in plane's tts::RobotVoiceFx and the C firmware): compressor
    (threshold 0.2, 4:1, makeup 1.5, 5 ms attack, 80 ms release), then a light reverb."""
    import math
    rate = 16000.0
    attack, release = math.exp(-1 / (0.005 * rate)), math.exp(-1 / (0.080 * rate))
    env = 0.0
    combs = [[[0.0] * n, 0, 0.0] for n in (401, 431, 461, 487)]
    aps = [[[0.0] * n, 0] for n in (199, 157)]
    fb, damp, wet = 0.70 + 0.2 * 0.20, 0.8, 0.05
    out = []
    for v in s:
        x = v / 32768.0
        m = abs(x)
        c = attack if m > env else release
        env = c * env + (1 - c) * m
        red = (env / 0.2) ** (1 / 4.0 - 1) if env > 0.2 else 1.0
        x = max(-1.0, min(32767 / 32768, x * red * 1.5))
        total = 0.0
        for cb in combs:
            buf, pos, store = cb
            o = buf[pos]
            store = o * (1 - damp) + store * damp
            buf[pos] = x + store * fb
            cb[1], cb[2] = (pos + 1) % len(buf), store
            total += o
        r = total / len(combs)
        for ap in aps:
            buf, pos = ap
            b = buf[pos]
            o = -r + b
            buf[pos] = r + b * 0.5
            ap[1] = (pos + 1) % len(buf)
            r = o
        y = x * (1 - wet) + r * wet
        out.append(max(-32768, min(32767, int(y * 32768))))
    return out


def encode(s):
    pred, idx = s[0], 0
    out = bytearray()
    nib = []
    for x in s:
        step = STEPS[idx]
        diff = x - pred
        code = 0
        if diff < 0:
            code, diff = 8, -diff
        delta = step >> 3
        if diff >= step:
            code |= 4
            diff -= step
            delta += step
        if diff >= step >> 1:
            code |= 2
            diff -= step >> 1
            delta += step >> 1
        if diff >= step >> 2:
            code |= 1
            delta += step >> 2
        pred = pred - delta if code & 8 else pred + delta
        pred = max(-32768, min(32767, pred))
        idx = max(0, min(88, idx + INDEX[code]))
        nib.append(code)
    if len(nib) % 2:
        nib.append(0)
    for i in range(0, len(nib), 2):
        out.append(nib[i] | (nib[i + 1] << 4))
    return out


def main():
    header = struct.pack("<H", len(PHRASES))
    table = b""
    body = b""
    base = 2 + 8 * len(PHRASES)
    total = 0.0
    for text in PHRASES:
        s = voice_fx(trim(synth(text)))
        blob = struct.pack("<hBx", s[0], 0) + encode(s)
        table += struct.pack("<II", base + len(body), len(s))
        body += blob
        total += len(s) / 16000
        print(f"{len(s) / 16000:5.2f} s  {text}")
    path = os.path.join(os.path.dirname(__file__), "..", "assets", "phrases.bin")
    with open(path, "wb") as f:
        f.write(header + table + body)
    print(f"{len(PHRASES)} phrases, {total:.1f} s, {len(header + table + body) // 1024} KB -> {os.path.normpath(path)}")


if __name__ == "__main__":
    sys.exit(main())

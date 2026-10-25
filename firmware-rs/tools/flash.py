#!/usr/bin/env python3
"""Flash the Rust firmware over USB, without holding the power button.

Normally the robot updates over Wi-Fi (`bbplane ota`); this is for a robot that cannot. Run from
the repository, with esptool (ESP-IDF's Python environment, or `pip install esptool`):

    python firmware-rs/tools/flash.py -p /dev/cu.usbserial-XXXX          # the app (after the first time)
    python firmware-rs/tools/flash.py -p /dev/cu.usbserial-XXXX --all    # bootloader and partition table too

The app goes to the first app slot and the OTA boot record is erased, so the robot boots what was
just flashed even after it updated itself into the other slot. --all also writes the bootloader
(bootloader/bootloader.bin, with the power-latch hook) and the partition table.

The robot powers itself off when its firmware boots without the power button held (the
hold-to-turn-on check), and esptool's own reset leaves it in its ROM bootloader. So this resets
into the bootloader once, flashes without further resets, and ends with a plain reset; the robot
then switches itself off. Turn it on as usual.
"""

import argparse
import os
import subprocess
import sys
import tempfile
import time

import serial

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
OUT = os.path.join(ROOT, "target", "xtensa-esp32s3-espidf", "release")


def reset(port: str, bootloader: bool) -> None:
    """Pulse EN via RTS, holding GPIO0 low via DTR as EN rises to select the ROM bootloader."""
    s = serial.Serial(port, 115200)
    s.dtr = False
    s.rts = True  # EN low
    time.sleep(0.1)
    if bootloader:
        s.dtr = True  # GPIO0 low...
    s.rts = False  # ...as EN rises
    time.sleep(0.05)
    s.dtr = False
    s.close()


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("-p", "--port", required=True)
    ap.add_argument("-b", "--baud", type=int, default=460800)
    ap.add_argument("--image", default=os.path.join(OUT, "butterbot-ai.bin"), help="app image (tools/build.sh makes it)")
    ap.add_argument("--all", action="store_true", help="also write the bootloader and the partition table")
    a = ap.parse_args()

    if not os.path.exists(a.image):
        print(f"{a.image} is missing: build it with firmware-rs/tools/build.sh")
        return 1
    images = [("0x20000", a.image)]
    if a.all:
        table = os.path.join(OUT, "partition-table.bin")
        if not os.path.exists(table):
            print(f"{table} is missing: build first")
            return 1
        images = [("0x0", os.path.join(ROOT, "bootloader", "bootloader.bin")), ("0x8000", table)] + images

    esptool = [sys.executable, "-m", "esptool"]
    if subprocess.call(esptool + ["version"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL) != 0:
        print(f"esptool is not available to {sys.executable}; run `. ~/esp/esp-idf/export.sh` or `pip install esptool`.")
        return 1

    # An empty OTA boot record: the bootloader then boots the first app slot.
    with tempfile.NamedTemporaryFile(suffix=".bin", delete=False) as f:
        f.write(b"\xff" * 0x2000)
        otadata = f.name
    images.append(("0xf000", otadata))

    reset(a.port, bootloader=True)
    # Underscore spellings: esptool 4.x (ESP-IDF 5.5) only knows these; esptool 5 accepts them.
    cmd = esptool + ["--chip", "esp32s3", "-p", a.port, "-b", str(a.baud), "--before", "no_reset",
                     "--after", "no_reset", "write_flash", "--flash_mode", "dio", "--flash_freq", "80m",
                     "--flash_size", "16MB"] + [x for pair in images for x in pair]
    print(" ".join(cmd), flush=True)
    rc = subprocess.call(cmd)
    os.unlink(otadata)
    # Leave the ROM bootloader either way, or the robot sits there with the light off.
    reset(a.port, bootloader=False)
    if rc != 0:
        print("\nFlashing failed. If the robot switched off while flashing, hold its power button and run this again.")
        return rc
    print("\nDone. The robot switches itself off now; turn it on as usual (hold the button for a second).")
    return 0


if __name__ == "__main__":
    sys.exit(main())

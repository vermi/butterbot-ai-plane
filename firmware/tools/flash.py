#!/usr/bin/env python3
"""Flash a built ButterBot firmware without holding the power button.

Run after `idf.py build`, from the firmware directory, with esptool installed
(`pip install esptool`, or use the one in ESP-IDF's environment):

    python tools/flash.py -p /dev/cu.usbserial-XXXX          # app only (after the first time)
    python tools/flash.py -p /dev/cu.usbserial-XXXX --all    # everything, as idf.py flash would

Why not plain `idf.py flash`: the robot powers itself off whenever its firmware boots without the
power button held (the stock hold-to-turn-on check). esptool's reset at the end of a flash also
leaves the chip in its ROM bootloader, with the light off and the button ignored. This script
resets into the bootloader once, flashes without further resets, and finishes with a plain reset
so the firmware boots and switches the robot off cleanly. Then turn it on as usual.
"""

import argparse
import os
import subprocess
import sys
import time

import serial


def reset(port: str, bootloader: bool) -> None:
    """Pulse EN via the adapter's RTS line, holding GPIO0 low via DTR when EN rises to select the
    ROM bootloader (esptool's classic auto-reset sequence)."""
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
    ap.add_argument("--build", default="build", help="ESP-IDF build directory (default: build)")
    ap.add_argument("--all", action="store_true", help="also write bootloader, partition table and data partitions")
    a = ap.parse_args()

    build = os.path.abspath(a.build)
    with open(os.path.join(build, "flash_args")) as f:
        lines = [l.split() for l in f.read().splitlines() if l.strip()]
    args = lines[0]  # --flash_mode dio --flash_freq 80m --flash_size 16MB
    images = [(addr, path) for addr, path in lines[1:]]
    if not a.all:
        images = [(addr, path) for addr, path in images if addr == "0x10000"]
    for addr, path in images:
        args += [addr, path]

    # Underscore spellings: ESP-IDF 5.5 installs esptool 4.x, which only knows these; esptool 5
    # still accepts them. Check esptool runs before touching the robot.
    esptool = [sys.executable, "-m", "esptool"]
    if subprocess.call(esptool + ["version"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL) != 0:
        print(f"esptool is not available to {sys.executable}; run `. ~/esp/esp-idf/export.sh` or `pip install esptool`.")
        return 1

    reset(a.port, bootloader=True)
    cmd = esptool + ["--chip", "esp32s3", "-p", a.port, "-b", str(a.baud),
                     "--before", "no_reset", "--after", "no_reset", "write_flash", *args]
    print(" ".join(cmd), flush=True)
    rc = subprocess.call(cmd, cwd=build)
    # Leave the ROM bootloader either way, or the robot sits there with the light off.
    reset(a.port, bootloader=False)
    if rc != 0:
        print("\nFlashing failed. If the robot switched off while flashing, hold its power button and run this again.")
        return rc
    print("\nDone. The robot switches itself off now; turn it on as usual (hold the button for a second).")
    return 0


if __name__ == "__main__":
    sys.exit(main())

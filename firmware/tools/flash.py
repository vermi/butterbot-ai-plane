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
    options = lines[0]  # --flash_mode dio --flash_freq 80m --flash_size 16MB
    images = [(addr, path) for addr, path in lines[1:]]
    if not a.all:
        images = [(addr, path) for addr, path in images if addr == "0x10000"]
    args = [x.replace("_", "-") if x.startswith("--") else x for x in options]
    for addr, path in images:
        args += [addr, path]

    reset(a.port, bootloader=True)
    cmd = [sys.executable, "-m", "esptool", "--chip", "esp32s3", "-p", a.port, "-b", str(a.baud),
           "--before", "no-reset", "--after", "no-reset", "write-flash", *args]
    print(" ".join(cmd), flush=True)
    rc = subprocess.call(cmd, cwd=build)
    if rc != 0:
        print("\nFlashing failed. If the robot switched off while flashing, hold its power button and run this again.")
        return rc
    reset(a.port, bootloader=False)
    print("\nDone. The robot switches itself off now; turn it on as usual (hold the button for a second).")
    return 0


if __name__ == "__main__":
    sys.exit(main())

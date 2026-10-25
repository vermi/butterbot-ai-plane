#!/usr/bin/env python3
"""Print the robot's serial console without resetting it.

macOS raises DTR and RTS when the port opens; with both asserted the auto-reset circuit leaves
EN and GPIO0 alone. Changing them one at a time (RTS alone asserted) pulls EN low: a reset, and
the firmware's power-on check then switches the robot off. So they are never touched here.

    python tools/serial_log.py /dev/cu.usbserial-110 [logfile]
"""
import sys
import time

import serial

port = sys.argv[1]
out = open(sys.argv[2], "a", buffering=1) if len(sys.argv) > 2 else sys.stdout
s = serial.Serial(port, 115200, timeout=1)
buf = b""
while True:
    data = s.read(4096)
    if not data:
        continue
    buf += data
    while b"\n" in buf:
        line, buf = buf.split(b"\n", 1)
        out.write(time.strftime("%H:%M:%S ") + line.decode("utf-8", "replace").rstrip("\r") + "\n")

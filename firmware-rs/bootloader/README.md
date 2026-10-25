# Bootloader

`bootloader.bin` is the robot's second-stage bootloader, as built from the C firmware (ESP-IDF
5.5.3, app rollback on, QIO flash) with the hook in `hooks.c`: it leaves PWDN (GPIO4) an input
first thing, so the robot's power latch stays on through the boot.

Updates over Wi-Fi never touch it. Only a full USB flash (`tools/flash.py --all`, e.g. a robot
still on the stock firmware) writes it, together with the partition table.

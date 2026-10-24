#ifndef BUTTERBOT_FIRMWARE_AIRESTART_H
#define BUTTERBOT_FIRMWARE_AIRESTART_H

/**
 * Restart the robot straight into AI mode. Used after Manual Override's RC mode: Wi-Fi needs a
 * large block of internal RAM, and after RC mode's own Wi-Fi and video feed the heap is too
 * fragmented to start it again; a restart starts over with a clean heap in a few seconds. The
 * power stays on across a software restart, and the power-on button check is skipped for it.
 */
[[noreturn]] void restartIntoAiMode();

/**
 * At boot: whether this start is such a restart (it is then cleared), or a reset after a crash
 * (panic or watchdog) while the robot was on: it stays on and goes back to AI mode then too. A
 * crash in a freshly updated firmware is the second boot that makes the bootloader roll back.
 * Crashing again and again (see aiModeWorked()) ends in the normal power-on check: off.
 */
bool takeRestartIntoAiMode();

/** AI mode reached the plane: crashes before this one no longer count as a crash loop. */
void aiModeWorked();

#endif //BUTTERBOT_FIRMWARE_AIRESTART_H

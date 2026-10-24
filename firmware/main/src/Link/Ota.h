#ifndef BUTTERBOT_FIRMWARE_OTA_H
#define BUTTERBOT_FIRMWARE_OTA_H

#include <cstdint>
#include <string>

/**
 * Firmware updates over Wi-Fi, from the plane (`bbplane ota`).
 *
 * Next to AI mode (camera, microphone, Bluetooth, Wi-Fi) there is not enough internal RAM left to
 * download an image, so the robot restarts into update mode: right after boot, before anything
 * else starts, it joins Wi-Fi, downloads the image over HTTP into the app slot not running, checks
 * it (size, SHA-256, ESP-IDF's own image check), makes it the boot slot, tells the plane how it
 * went, and restarts into AI mode on it. Nothing else runs meanwhile; on any failure it restarts
 * into AI mode on the firmware it had.
 *
 * The new firmware has to prove itself: it is only kept once it has reached the plane again
 * (confirm()); a reset before that boots the previous one (bootloader rollback).
 */
namespace Ota {
	/** Keep the job and restart into update mode. Stop the motors first. */
	[[noreturn]] void restartIntoUpdate(const std::string& url, uint32_t size, const std::string& sha256);
	/** At boot, first thing (NVS must be up): run update mode if an update is pending. It ends in a
	 *  restart, so this only returns when there is nothing to do. */
	void runPending();
	/** Keep the running firmware if it was on probation (the plane is reachable). */
	void confirm();
	/** The running firmware's build id: the start of its ELF's SHA-256, as in the image. */
	const char* build();
}

#endif //BUTTERBOT_FIRMWARE_OTA_H

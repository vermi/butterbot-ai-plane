#ifndef BUTTERBOT_COMMON_CONTROLLERDATA_H
#define BUTTERBOT_COMMON_CONTROLLERDATA_H

#include <cstdint>

struct Ctrl {
	enum Command {
		EnterRC, ExitRC, Listen, Summon, ShutUp, Poke, Drive, RCSound, Scenario,
		LinkConfig, // AI control plane provisioning; never sent by the handheld controller
		LinkWake,   // Join Wi-Fi and enter AI mode with the stored configuration
		LinkTune    // Internal-RAM thresholds for starting Wi-Fi (LinkTuneData)
	} type;
};

struct DriveData {
	int8_t joystickX; // [-100, 100]
	int8_t joystickY; // [-100, 100]
};

struct RCData {
	char SSID[16];
	char password[16];
};

/**
 * Wi-Fi network and control plane address for the AI link (Ctrl::LinkConfig payload).
 * Layout is mirrored by bbproto::provision on the laptop: three NUL-terminated strings, then the port.
 */
struct LinkConfigData {
	char SSID[32];
	char password[64];
	char host[64];
	uint16_t port;
};
static_assert(sizeof(LinkConfigData) == 162, "LinkConfigData layout is part of the provisioning protocol");

/**
 * Ctrl::LinkTune payload: minimum free internal DMA RAM and largest free block (bytes) required
 * before the AI link starts Wi-Fi. Zero keeps the current value.
 */
struct LinkTuneData {
	uint32_t minFree;
	uint32_t minBlock;
};
static_assert(sizeof(LinkTuneData) == 8, "LinkTuneData layout is part of the provisioning protocol");

#endif //BUTTERBOT_COMMON_CONTROLLERDATA_H
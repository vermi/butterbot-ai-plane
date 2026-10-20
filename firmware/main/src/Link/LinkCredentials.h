#ifndef BUTTERBOT_FIRMWARE_LINKCREDENTIALS_H
#define BUTTERBOT_FIRMWARE_LINKCREDENTIALS_H

#include <CtrlData.h>

/**
 * Persists the AI link configuration (home Wi-Fi + control plane address) in its own NVS
 * namespace, separate from the Settings blob so that blob's layout stays untouched.
 */
class LinkCredentials {
public:
	static bool load(LinkConfigData& out);
	static bool store(const LinkConfigData& data);
	static bool isValid(const LinkConfigData& data);

	/**
	 * Crash latch around Wi-Fi start-up: set before esp_wifi_init(), cleared once it succeeded.
	 * Still set at boot means the previous attempt took the firmware down.
	 */
	static void setWiFiStarting(bool starting);
	static bool wasWiFiStarting();

	static bool loadTune(LinkTuneData& out);
	static bool storeTune(const LinkTuneData& data);

	/** Wi-Fi channel the network was last joined on (0: unknown). */
	static uint8_t loadChannel();
	static void storeChannel(uint8_t channel);
};

#endif //BUTTERBOT_FIRMWARE_LINKCREDENTIALS_H

#ifndef BUTTERBOTCTRL_FIRMWARE_HOMEWIFI_H
#define BUTTERBOTCTRL_FIRMWARE_HOMEWIFI_H

#include <cstdint>

/**
 * The home Wi-Fi, joined next to the controller's own access point (which RC mode needs) while
 * the AI mode camera view is open. The details come from the robot each time and are kept in RAM
 * only, never in flash.
 */
namespace HomeWiFi {

enum class State : uint8_t { Off, Joining, Joined, Failed };

void join(const char* ssid, const char* password);

/** Back to the access point alone, on its own channel; the details are forgotten. */
void leave();

State state();

}

#endif //BUTTERBOTCTRL_FIRMWARE_HOMEWIFI_H

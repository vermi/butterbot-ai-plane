#ifndef BUTTERBOT_FIRMWARE_BONDS_H
#define BUTTERBOT_FIRMWARE_BONDS_H

#include <esp_bt_defs.h>

// Missing from the public firmware repository; reconstructed from its call sites.
namespace BLE {

/**
 * @return true if addr is in Bluedroid's bonded device list.
 * Peers that connect with a resolvable private address only match once Bluedroid has
 * resolved them to their identity address.
 */
bool isBonded(const esp_bd_addr_t addr);

}

#endif //BUTTERBOT_FIRMWARE_BONDS_H

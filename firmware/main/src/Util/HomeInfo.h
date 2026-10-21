#ifndef BUTTERBOT_FIRMWARE_HOMEINFO_H
#define BUTTERBOT_FIRMWARE_HOMEINFO_H

#include <BBData.h>

/**
 * What the handheld controller's home screen shows (HomeInfoData): the robot's battery and clock,
 * and in AI mode the plane's latest news. Kept here so every state sends the same thing.
 */
namespace HomeInfo {

/** The plane's news (next reminder, latest sighting, last line said); the clock part is ignored. */
void setAi(const HomeInfoData& news);

/** Leaving AI mode: no more news. */
void clearAi();

/** Set the robot's clock from the plane's when it is unset or more than a few seconds off. */
void syncClock(const HomeInfoData& plane);

/** Send it all to the controller (a no-op while none is connected). */
void send();

}

#endif //BUTTERBOT_FIRMWARE_HOMEINFO_H

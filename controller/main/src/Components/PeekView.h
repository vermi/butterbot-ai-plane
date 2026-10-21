#ifndef BUTTERBOTCTRL_FIRMWARE_PEEKVIEW_H
#define BUTTERBOTCTRL_FIRMWARE_PEEKVIEW_H

#include <vector>
#include <LV_Interface/LVObject.h>
#include <BBData.h>
#include "Services/ThemeService.h"
#include "Util/Feed.h"

/**
 * The robot's camera on the controller in AI mode (the joystick pushed up on the home screen).
 * Bluetooth is too slow for video: the robot hands over the home Wi-Fi and the plane's address
 * (PeekWiFiData), the controller joins the Wi-Fi next to its own access point and says hello to the
 * plane every second, and the plane streams frames in the RC feed's format, which the RC video
 * code (Feed) decodes. What is in view (PeekCaption, via the robot) shows underneath.
 * A screen-wide overlay under the top bar; deleting it leaves the Wi-Fi again.
 */
class PeekView : public LVObject {
public:
	explicit PeekView(lv_obj_t* parent);
	~PeekView() override;

	void loop();
	void onWiFi(const std::vector<uint8_t>& data);
	void onCaption(const std::vector<uint8_t>& data);

private:
	static constexpr int32_t W = 128;
	static constexpr int32_t H = 96;
	static constexpr uint32_t HelloMs = 1000;
	static constexpr uint32_t WiFiDetailsWaitMs = 6000;
	static constexpr uint32_t JoinWaitMs = 20000;

	ThemeService* theme;
	StrongObjectPtr<Feed> feed;
	uint8_t* pixels = nullptr;
	lv_image_dsc_t dsc{};
	lv_obj_t* img;
	lv_obj_t* caption;
	lv_obj_t* status;

	uint32_t planeIp = 0;
	int sock = -1;
	uint64_t openedAt = 0;
	uint64_t joinedAt = 0;
	uint64_t lastHello = 0;
	bool gotFrame = false;
	uint64_t lastLog = 0;

	void setStatus(const char* text);
	void sayHello();
};

#endif //BUTTERBOTCTRL_FIRMWARE_PEEKVIEW_H

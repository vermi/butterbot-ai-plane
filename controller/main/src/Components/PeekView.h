#ifndef BUTTERBOTCTRL_FIRMWARE_PEEKVIEW_H
#define BUTTERBOTCTRL_FIRMWARE_PEEKVIEW_H

#include <vector>
#include <LV_Interface/LVObject.h>
#include "Services/ThemeService.h"
#include "esp_jpeg_dec.h"

/**
 * The robot's camera on the controller in AI mode (the joystick pushed up on the home screen).
 * The plane shrinks the robot's frames to small JPEGs; the robot passes them on over Bluetooth in
 * pieces (BB::Action::AI::PeekChunk), with what is in view (PeekCaption) shown underneath.
 * A screen-wide overlay under the top bar; the owner opens and closes the stream.
 */
class PeekView : public LVObject {
public:
	explicit PeekView(lv_obj_t* parent);
	~PeekView() override;

	void onChunk(const std::vector<uint8_t>& data);
	void onCaption(const std::vector<uint8_t>& data);

private:
	static constexpr int32_t W = 128;
	static constexpr int32_t H = 96;

	ThemeService* theme;
	jpeg_dec_handle_t dec = nullptr;
	uint8_t* pixels = nullptr; // RGB565, 16-byte aligned for the decoder
	lv_image_dsc_t dsc{};
	lv_obj_t* img;
	lv_obj_t* caption;
	lv_obj_t* status;

	// The picture being put together
	std::vector<uint8_t> jpeg;
	std::vector<bool> got;
	size_t gotCount = 0;
	int16_t frame = -1;

	void decode();
};

#endif //BUTTERBOTCTRL_FIRMWARE_PEEKVIEW_H

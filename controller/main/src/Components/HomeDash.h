#ifndef BUTTERBOTCTRL_FIRMWARE_HOMEDASH_H
#define BUTTERBOTCTRL_FIRMWARE_HOMEDASH_H

#include <LV_Interface/LVObject.h>
#include "Services/ThemeService.h"
#include "Services/RobotState.h"
#include "Services/Battery.h"

/**
 * The home screen when nothing else is on it: the robot's clock, both batteries, and in AI mode
 * what the agent is up to and its latest news (the next reminder, the latest thing it noticed,
 * the last thing it said). Outside AI mode, a few button hints instead.
 * Floating like GuideElement: ignores the parent's layout and scroll. The owner toggles
 * LV_OBJ_FLAG_HIDDEN and calls loop().
 */
class HomeDash : public LVObject {
public:
	explicit HomeDash(lv_obj_t* parent);

	/* Keeps the clock running; call every frame */
	void loop();

private:
	ThemeService* theme;
	RobotState* robotState;
	Battery* battery;

	lv_obj_t* clock;
	lv_obj_t* date;
	lv_obj_t* botPct;
	lv_obj_t* botBar;
	lv_obj_t* ctrlPct;
	lv_obj_t* ctrlBar;
	lv_obj_t* mode;
	lv_obj_t* activity;
	lv_obj_t* next;
	lv_obj_t* seen;
	lv_obj_t* seenAge;
	lv_obj_t* said;
	lv_obj_t* hints;

	static constexpr uint32_t RefreshMs = 250;
	uint64_t lastRefresh = 0;

	void buildUI();
	void refresh();
	void showClock(const HomeInfoData& home, uint64_t homeAt);
	void showBatteries(const HomeInfoData& home, uint64_t homeAt);
	void showAi(const HomeInfoData& home, uint64_t homeAt);

	lv_obj_t* addLabel(int32_t x, int32_t y, int32_t w, lv_text_align_t align, lv_color_t color);
	lv_obj_t* addBar(int32_t y);
	/** Set a label's text only when it changed: setting it restarts a scrolling label. */
	static void setText(lv_obj_t* label, const char* text);
	static void setBar(lv_obj_t* bar, uint8_t percent);
};


#endif //BUTTERBOTCTRL_FIRMWARE_HOMEDASH_H

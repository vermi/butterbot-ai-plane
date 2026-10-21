#ifndef BUTTERBOTCTRL_FIRMWARE_HOMEDASH_H
#define BUTTERBOTCTRL_FIRMWARE_HOMEDASH_H

#include <LV_Interface/LVObject.h>
#include "Services/ThemeService.h"
#include "Services/RobotState.h"
#include "Services/Battery.h"
#include "Components/BotFace.h"

/**
 * The home screen when nothing else is on it: Butter Bot's face, with his mood, what he is doing,
 * his pace, his battery and the phone's music beside it. Below, in AI mode, the next reminder (or
 * his own next plan), the latest thing he noticed, and a speech bubble: what he is saying as he
 * says it, or what was just said to him. Outside AI mode, a few button hints instead. The
 * controller's own battery shows only when its batteries run low (on USB it has none to show).
 * Floating like GuideElement: ignores the parent's layout and scroll. The owner toggles
 * LV_OBJ_FLAG_HIDDEN and calls loop().
 */
class HomeDash : public LVObject {
public:
	explicit HomeDash(lv_obj_t* parent);

	/* Animates the face and refreshes the text; call every frame */
	void loop();

	/** Show a new activity level for a moment (the joystick's quick change). */
	void showActivity(AIStatusData::Activity level);

	BotFace* getFace() const{ return face; }

private:
	ThemeService* theme;
	RobotState* robotState;
	Battery* battery;

	BotFace* face;
	lv_obj_t* mood;
	lv_obj_t* mode;
	lv_obj_t* activity;
	lv_obj_t* botPct;
	lv_obj_t* botBar;
	lv_obj_t* extra; // the phone's music, or the controller's low batteries
	lv_obj_t* next;
	lv_obj_t* seen;
	lv_obj_t* seenAge;
	lv_obj_t* bubble;
	lv_obj_t* bubbleText;
	lv_obj_t* hints;
	std::string bubbleShown;
	uint64_t bubbleChangedAt = 0;

	static constexpr uint32_t RefreshMs = 250;
	uint64_t lastRefresh = 0;
	static constexpr uint32_t ActivityShowMs = 1800;
	uint64_t activityShownUntil = 0;
	AIStatusData::Activity activityShown = AIStatusData::Activity::Normal;
	std::string lastSeen;

	void buildUI();
	void refresh();
	void showBatteries(const HomeInfoData& home, uint64_t homeAt);
	void showAi(const HomeInfoData& home, uint64_t homeAt);

	lv_obj_t* addLabel(int32_t x, int32_t y, int32_t w, lv_text_align_t align, lv_color_t color);
	lv_obj_t* addBar(int32_t x, int32_t y, int32_t w);
	void showBubble(const HomeInfoData& home);
	/** Set a label's text only when it changed: setting it restarts a scrolling label. */
	static void setText(lv_obj_t* label, const char* text);
	static void setBar(lv_obj_t* bar, uint8_t percent);
};


#endif //BUTTERBOTCTRL_FIRMWARE_HOMEDASH_H

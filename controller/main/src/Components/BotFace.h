#ifndef BUTTERBOTCTRL_FIRMWARE_BOTFACE_H
#define BUTTERBOTCTRL_FIRMWARE_BOTFACE_H

#include <LV_Interface/LVObject.h>
#include "Services/ThemeService.h"
#include "Services/RobotState.h"

/**
 * Butter Bot's face in AI mode, as pixel art: his mood (the agent's own emotional state) with what
 * he is doing on top of it. He blinks, glances about, moves his mouth while he talks, looks up
 * while he thinks, half-closes his eyes in focus mode and sleeps with no agent running. A poke
 * makes him wince and a new sighting makes him look surprised, for a moment.
 */
class BotFace : public LVObject {
public:
	explicit BotFace(lv_obj_t* parent);

	static constexpr int32_t Size = 30;  // drawn on a 30x30 grid...
	static constexpr int32_t Scale = 2;  // ...of 2x2 pixels

	/* Animates; call every frame */
	void loop();

	/** React for a moment. */
	void poked();
	void startled();

private:
	ThemeService* theme;
	RobotState* robotState;
	lv_obj_t* canvas;
	alignas(16) uint16_t buf[Size * Scale * Size * Scale];
	uint16_t ink;
	uint16_t paper;

	enum class Eye : uint8_t { Open, Closed, Happy, Lidded, Squint };
	enum class Mouth : uint8_t { Flat, Smile, Grin, Frown, O, Talk, Wavy, Smirk };
	enum class Brows : uint8_t { None, Raised, Angry, Sad };

	struct Look {
		Eye left = Eye::Open, right = Eye::Open;
		int8_t eyeW = 5, eyeH = 7;
		int8_t rightEyeH = 0; // 0: same as the left
		int8_t dx = 0, dy = 0; // where he looks
		Mouth mouth = Mouth::Smile;
		int8_t mouthW = 8, mouthH = 2;
		Brows brows = Brows::None;
		int8_t bob = 0;
		bool zzz = false;

		bool operator==(const Look&) const = default;
	};

	Look shown{};
	bool drawn = false;

	// Animation clocks (millis)
	uint64_t nextBlink = 0, blinkUntil = 0;
	uint64_t nextGlance = 0;
	int8_t glanceX = 0, glanceY = 0;
	uint64_t nextTalk = 0;
	int8_t talkH = 2;
	uint64_t pokedUntil = 0, startledUntil = 0;

	Look compose(uint64_t now);
	void draw(const Look& l);

	void px(int32_t x, int32_t y, bool on = true);
	void rect(int32_t x, int32_t y, int32_t w, int32_t h);
	void line(int32_t x0, int32_t y0, int32_t x1, int32_t y1);
	void drawEye(Eye e, int32_t cx, int32_t cy, int32_t w, int32_t h, bool left);
	void drawMouth(Mouth m, int32_t cx, int32_t cy, int32_t w, int32_t h);
};

#endif //BUTTERBOTCTRL_FIRMWARE_BOTFACE_H

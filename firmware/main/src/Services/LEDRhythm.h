#ifndef BUTTERBOT_FIRMWARE_LEDRHYTHM_H
#define BUTTERBOT_FIRMWARE_LEDRHYTHM_H

#include <algorithm>
#include <cmath>
#include <cstdint>
#include <Services/LED/LED.h>
#include <Util/stdafx.h>

/**
 * How the status LED blinks. It hangs off an I/O expander pin, which has three levels: off, dim
 * (any level below 0.5, see OutputTCA) and bright. The LED sits at `base` and every period starts
 * with `count` flashes to `flash` of `onMs`, `offMs` apart. `count` 0 is steady at `base`.
 */
struct LEDRhythm {
	uint16_t onMs = 0;
	uint16_t offMs = 0;
	uint8_t count = 0;
	uint16_t periodMs = 1000;
	float base = 0.0f;
	float flash = 1.0f;

	static constexpr float Dim = 0.2f;
	static constexpr float Bright = 1.0f;

	static constexpr LEDRhythm steady(float level){
		return { 0, 0, 0, 1000, level, level };
	}

	bool operator==(const LEDRhythm&) const = default;

	/** Flashes long enough to see and to tick, all inside the period. */
	LEDRhythm sane() const{
		LEDRhythm r = *this;
		if(r.count == 0){
			return r;
		}
		r.count = std::min<uint8_t>(r.count, 5);
		r.onMs = std::clamp<uint16_t>(r.onMs, MinMs, 5000);
		r.offMs = r.count > 1 ? std::clamp<uint16_t>(r.offMs, MinMs, 5000) : std::min<uint16_t>(r.offMs, 5000);
		const uint32_t busy = (uint32_t) (r.onMs + r.offMs) * r.count;
		r.periodMs = (uint16_t) std::clamp<uint32_t>(r.periodMs, busy, 60000);
		return r;
	}

	static constexpr uint16_t MinMs = 20; // the LED service ticks at 10 ms at best
};

/** Plays an LEDRhythm forever, waking the LED service only when the LED changes. */
template<typename LED>
class LEDRhythmFunction : public LEDFunction<LED, float> {
public:
	explicit LEDRhythmFunction(const LEDRhythm& rhythm) : rhythm(rhythm.sane()){}

	float getValue() const noexcept override{
		return lit ? rhythm.flash : rhythm.base;
	}

	bool isDone() const noexcept override{
		return false;
	}

	TickType_t getInterval() const noexcept override{
		return started ? untilChange / portTICK_PERIOD_MS : 0;
	}

private:
	void tick(float deltaTime) noexcept override{
		// Absolute time, like LEDBlinkFunction: the service sleeps between changes.
		const uint64_t now = millis();
		if(!started){
			started = true;
			startTime = now;
		}
		if(rhythm.count == 0){
			lit = false;
			untilChange = 1000;
			return;
		}
		const uint32_t t = (now - startTime) % rhythm.periodMs;
		const uint32_t slot = rhythm.onMs + rhythm.offMs;
		bool inFlash = false;
		uint32_t next;
		if(t < slot * rhythm.count){
			const uint32_t inSlot = t % slot;
			inFlash = inSlot < rhythm.onMs;
			next = inFlash ? rhythm.onMs - inSlot : slot - inSlot;
		}else{
			next = rhythm.periodMs - t;
		}
		lit = inFlash;
		untilChange = std::max<uint32_t>(next, 1);
	}

	const LEDRhythm rhythm;
	uint64_t startTime = 0;
	uint32_t untilChange = 0;
	bool started = false;
	bool lit = false;
};

/**
 * A soft glow for the LED add-on card, which has real dimming (PWM): breathes between `low` and
 * `high` (duty, 0..1) once every `periodMs`, or stays at `high` when `periodMs` is 0. The card is
 * very bright: its stock "on" is 0.1.
 */
struct LEDGlow {
	float low = 0.0f;
	float high = 0.0f;
	uint16_t periodMs = 0;

	bool steady() const{
		return periodMs == 0 || low == high;
	}

	bool operator==(const LEDGlow&) const = default;
};

/** Plays a breathing LEDGlow forever, evenly in perceived brightness rather than in duty. */
template<typename LED>
class LEDGlowFunction : public LEDFunction<LED, float> {
public:
	explicit LEDGlowFunction(const LEDGlow& glow) : glow(glow){}

	float getValue() const noexcept override{
		return value;
	}

	bool isDone() const noexcept override{
		return false;
	}

	TickType_t getInterval() const noexcept override{
		return started ? 20 / portTICK_PERIOD_MS : 0;
	}

private:
	void tick(float deltaTime) noexcept override{
		const uint64_t now = millis();
		if(!started){
			started = true;
			startTime = now;
		}
		const float phase = (float) ((now - startTime) % std::max<uint16_t>(glow.periodMs, 1)) / std::max<uint16_t>(glow.periodMs, 1);
		const float t = 0.5f - 0.5f * cosf(2.0f * (float) M_PI * phase);
		const float a = sqrtf(glow.low);
		const float b = sqrtf(glow.high);
		const float r = a + (b - a) * t;
		value = r * r;
	}

	const LEDGlow glow;
	uint64_t startTime = 0;
	bool started = false;
	float value = 0.0f;
};

#endif //BUTTERBOT_FIRMWARE_LEDRHYTHM_H

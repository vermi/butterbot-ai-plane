#ifndef BUTTERBOT_FIRMWARE_JOYSTICK_H
#define BUTTERBOT_FIRMWARE_JOYSTICK_H

#include <algorithm>
#include <cmath>
#include <cstdint>
#include <utility>

/**
 * Motor speeds (left, right) for the handheld controller's joystick in RC mode.
 *
 * A continuous mix: forward/back from the stick's height, turning from its sideways push, so
 * every angle in between gives its own curve (stock snapped the stick to eight directions, and
 * diagonals came out as one fixed gentle curve). Straight sideways spins in place. Reversing
 * steers like a car backing up: stick back and right, the rear swings right, as stock did.
 * Half a push is already full speed, as in stock.
 */
inline std::pair<int8_t, int8_t> joystickMotors(int8_t joystickX, int8_t joystickY){
	const float x = std::clamp(joystickX / 100.0f, -1.0f, 1.0f);
	const float y = std::clamp(joystickY / 100.0f, -1.0f, 1.0f);
	const float push = std::sqrt(x * x + y * y);
	static constexpr float DeadZone = 0.08f;
	if(push < DeadZone){
		return { 0, 0 };
	}

	float left = y >= 0 ? y + x : y - x;
	float right = y >= 0 ? y - x : y + x;

	// The faster track runs at the stick's push (doubled, capped), the other in proportion.
	const float larger = std::max(std::abs(left), std::abs(right));
	const float speed = std::min(1.0f, push * 2.0f) * 100.0f;
	left = left / larger * speed;
	right = right / larger * speed;
	return { static_cast<int8_t>(std::lround(left)), static_cast<int8_t>(std::lround(right)) };
}

#endif //BUTTERBOT_FIRMWARE_JOYSTICK_H

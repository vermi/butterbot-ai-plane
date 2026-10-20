#ifndef BUTTERBOT_FIRMWARE_JOYSTICK_H
#define BUTTERBOT_FIRMWARE_JOYSTICK_H

#include <algorithm>
#include <cmath>
#include <cstdint>
#include <utility>

/**
 * Motor speeds (left, right) for the handheld controller's joystick, as stock RC mode drives:
 * eight directions (straight, gentle curves, spin in place), speed from how far it is pushed.
 */
inline std::pair<int8_t, int8_t> joystickMotors(int8_t joystickX, int8_t joystickY){
	if(joystickX == 0 && joystickY == 0){
		return { 0, 0 };
	}

	static constexpr float MaxSpeed = 100.0f;

	const float x = joystickX;
	const float y = joystickY;
	const float speed = std::clamp(std::sqrt(x * x + y * y), 0.0f, MaxSpeed);

	// Clockwise from straight ahead, 0..360.
	float angle = std::atan2(x, y) * 180.0f / static_cast<float>(M_PI);
	if(angle < 0){
		angle += 360.0f;
	}

	static constexpr float circParts = 360.0f / 8.0f;
	float calcAngle = angle + circParts / 2.0f;
	if(calcAngle >= 360){
		calcAngle -= 360.0f;
	}
	static constexpr float Left[8] = { 1.0f, 1.0f, 1.0f, -1.0f, -1.0f, -0.2f, -1.0f, 0.2f };
	static constexpr float Right[8] = { 1.0f, 0.2f, -1.0f, -0.2f, -1.0f, -1.0f, 1.0f, 1.0f };
	const uint8_t number = std::min<uint8_t>(std::floor(calcAngle / circParts), 7);

	const float left = std::clamp(Left[number] * speed * 2.0f, -100.0f, 100.0f);
	const float right = std::clamp(Right[number] * speed * 2.0f, -100.0f, 100.0f);
	return { static_cast<int8_t>(left), static_cast<int8_t>(right) };
}

#endif //BUTTERBOT_FIRMWARE_JOYSTICK_H

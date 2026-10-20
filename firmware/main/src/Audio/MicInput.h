#ifndef CMF_TEMPLATE_MICINPUT_H
#define CMF_TEMPLATE_MICINPUT_H

#include "Periphery/I2S.h"

class MicInput {
public:
	/**
	 * @param i2s - read-capable I2S instance
	 */
	MicInput(I2S* i2s = nullptr);

	/**
	 * Reads and returns a stereo audio buffer
	 * @param output buffer
	 * @return amount of read samples, or ESP_FAIL
	 */
	int32_t getStereo(std::span<int16_t> output);

	/**
	 * Reads and flattens audio into a mono buffer, high-pass filtered (see HighPassHz).
	 * @param output buffer
	 * @return amount of read samples, or ESP_FAIL
	 */
	int32_t getMono(std::span<int16_t> output);

private:
	I2S* i2s;

	static constexpr uint8_t BytesPerSample = sizeof(int16_t);

	static constexpr size_t BufferSize = 8 * 1024; //bytes, not samples

	std::array<int16_t, BufferSize/2> buf{};

	/**
	 * The microphone carries a large offset and rumble below 100 Hz: on the test robot it was
	 * nearly all of the background level, while speech was 25-35 dB above the noise in its own
	 * band. The ESP32-S3's PDM receiver has no high-pass filter of its own (later chips do), so
	 * getMono runs a second-order Butterworth high-pass at this cut-off.
	 */
	static constexpr float HighPassHz = 120.0f;
	float hpB[3] = {};
	float hpA[2] = {};
	float hpX[2] = {};
	float hpY[2] = {};
	int16_t highPass(float x);

};


#endif //CMF_TEMPLATE_MICINPUT_H

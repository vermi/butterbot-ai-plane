#include "MicInput.h"
#include <algorithm>
#include <cmath>

DEFINE_LOG(MicInput)

MicInput::MicInput(I2S* i2s) : i2s(i2s){
	if(!i2s){
		CMF_LOG(MicInput, LogLevel::Error, "No I2S peripheral provided!");
		abort();
	}

	// RBJ audio EQ cookbook high-pass, Q = 1/sqrt(2), for the 16 kHz microphone stream.
	constexpr float SampleRate = 16000.0f;
	const float w0 = 2.0f * (float) M_PI * HighPassHz / SampleRate;
	const float alpha = std::sin(w0) / (2.0f * (float) M_SQRT1_2);
	const float a0 = 1.0f + alpha;
	const float c = std::cos(w0);
	hpB[0] = (1.0f + c) / 2.0f / a0;
	hpB[1] = -(1.0f + c) / a0;
	hpB[2] = (1.0f + c) / 2.0f / a0;
	hpA[0] = -2.0f * c / a0;
	hpA[1] = (1.0f - alpha) / a0;
}

int16_t MicInput::highPass(float x){
	const float y = hpB[0] * x + hpB[1] * hpX[0] + hpB[2] * hpX[1] - hpA[0] * hpY[0] - hpA[1] * hpY[1];
	hpX[1] = hpX[0];
	hpX[0] = x;
	hpY[1] = hpY[0];
	hpY[0] = y;
	return (int16_t) std::clamp(y, -32768.0f, 32767.0f);
}

int32_t MicInput::getMono(std::span<int16_t> output){
	size_t totalSamplesRead = 0;

	while(totalSamplesRead < output.size()){
		const auto readSamples = i2s->read((uint8_t*) buf.data(), BufferSize) / BytesPerSample;

		if(!readSamples){
			break;
		}

		for(size_t i = 0; i < (output.size() - totalSamplesRead) && i < (readSamples / 2); i++){
			output[totalSamplesRead + i] = highPass((buf[i * 2] + buf[i * 2 + 1]) / 2.0f);
		}

		totalSamplesRead += readSamples / 2;
	}

	return (int32_t) totalSamplesRead;
}

int32_t MicInput::getStereo(std::span<int16_t> output){
	const auto readSamples = i2s->read((uint8_t*) output.data(), output.size_bytes()) / BytesPerSample;

	if(!readSamples){
		return ESP_FAIL;
	}

	return (int32_t) readSamples;
}

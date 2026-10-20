#include "AiRestart.h"
#include <cstdint>
#include <esp_attr.h>
#include <esp_system.h>

namespace {
	constexpr uint32_t Magic = 0xB0B0A1A1;
	// Survives a software restart (not a power cycle); garbage after power-up, hence the magic.
	RTC_NOINIT_ATTR uint32_t restartIntoAi;
}

void restartIntoAiMode(){
	restartIntoAi = Magic;
	esp_restart();
}

bool takeRestartIntoAiMode(){
	const bool yes = esp_reset_reason() == ESP_RST_SW && restartIntoAi == Magic;
	restartIntoAi = 0;
	return yes;
}

#include "AiRestart.h"
#include <cstdint>
#include <esp_attr.h>
#include <esp_system.h>

namespace {
	constexpr uint32_t Magic = 0xB0B0A1A1;
	// Survives a software restart (not a power cycle); garbage after power-up, hence the magic.
	RTC_NOINIT_ATTR uint32_t restartIntoAi;
	// Crash resets in a row without AI mode reaching the plane in between; past the limit a crash
	// switches the robot off (the power-on check) instead of looping.
	RTC_NOINIT_ATTR uint32_t crashMagic;
	RTC_NOINIT_ATTR uint32_t crashes;
	constexpr uint32_t MaxCrashRestarts = 2;
}

void restartIntoAiMode(){
	restartIntoAi = Magic;
	esp_restart();
}

bool takeRestartIntoAiMode(){
	const esp_reset_reason_t why = esp_reset_reason();
	const bool crashed = why == ESP_RST_PANIC || why == ESP_RST_INT_WDT || why == ESP_RST_TASK_WDT || why == ESP_RST_WDT;
	crashes = crashed ? (crashMagic == Magic ? crashes : 0) + 1 : 0;
	crashMagic = Magic;
	const bool yes = (crashed && crashes <= MaxCrashRestarts) || (why == ESP_RST_SW && restartIntoAi == Magic);
	restartIntoAi = 0;
	return yes;
}

void aiModeWorked(){
	crashes = 0;
}

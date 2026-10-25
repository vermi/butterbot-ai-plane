#ifndef BUTTERBOTCTRL_FIRMWARE_PAIRINGSCREEN_H
#define BUTTERBOTCTRL_FIRMWARE_PAIRINGSCREEN_H

#include <LV_Interface/LVScreen.h>
#include <Services/ButtonInput.h>
#include "Enums.hpp"
#include "BLE/GAP.h"
#include "Services/Com.h"
#include "Services/LEDController.h"

class PairingScreen : public LVScreen {
public:
	PairingScreen();
	~PairingScreen() override;

private:
	BLE::GAP* gap;
	Com* com;
	LEDController* ledController;
	ButtonInput* buttonInput;

	void loop() override;

	static constexpr uint64_t ConnectHangMs = 25000;
	static constexpr uint64_t RelinkRestartMs = 30000;
	uint64_t shownAt = 0;

	void buildSettingsHint();

	void handleButtonEvent(Button btn, ButtonInput::Action action);
};


#endif //BUTTERBOTCTRL_FIRMWARE_PAIRINGSCREEN_H

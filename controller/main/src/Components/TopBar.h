#ifndef BUTTERBOTCTRL_FIRMWARE_TOPBAR_H
#define BUTTERBOTCTRL_FIRMWARE_TOPBAR_H

#include <LV_Interface/LVObject.h>
#include "Services/ThemeService.h"
#include "Components/SignalElement.h"
#include "Components/ShutUpElement.h"
#include "Services/Com.h"
#include "Services/RobotState.h"

class TopBar : public LVObject {
public:
	/**
	 * UI element for top bar with all containing elements
	 * @param parent parent LVGL object
	 */
	TopBar(lv_obj_t* parent);
	~TopBar() override;

	/* Main update loop, called by the parent */
	void loop();

	void setRobotElementsVisible(bool visible) const;

private:
	ThemeService* theme;
	Com* com;
	RobotState* robotState;

	SignalElement* signalEl;
	ShutUpElement* shutupEl;
	lv_obj_t* aiLabel;
	lv_obj_t* clockLabel;
	uint64_t lastClock = 0;
	// A lightning bolt while the robot is plugged in
	lv_obj_t* bolt;
	alignas(4) uint16_t boltBuf[5 * 7];

	void updateShutUpEl() const;
	void updateAi(const std::vector<uint8_t>& data) const;
	void showAi() const;

	void buildUI();
};


#endif //BUTTERBOTCTRL_FIRMWARE_TOPBAR_H

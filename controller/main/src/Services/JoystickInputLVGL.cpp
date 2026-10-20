#include "JoystickInputLVGL.h"
#include <cstdlib>

JoystickInputLVGL::JoystickInputLVGL(Joystick* joystick, ButtonInput* bi, Enum<int> enterButton) :
		Super(bi, {}, {}), joystick(joystick), bi(bi), enterButton(enterButton){
	lv_indev_set_type(getIndev(), LV_INDEV_TYPE_KEYPAD);
	lv_indev_set_read_cb(getIndev(), [](lv_indev_t* drv, lv_indev_data_t* data){
		static_cast<JoystickInputLVGL*>(InputLVGL::getInstance())->readJoystick(data);
	});
}

void JoystickInputLVGL::postInitProperties() noexcept{
	bi->OnButtonEvent.bind(this, &JoystickInputLVGL::onButtonInput);
}

void JoystickInputLVGL::readJoystick(lv_indev_data_t* data){
	lv_key_t currentKey = static_cast<lv_key_t>(0);
	bool pressed = false;

	if(enterPressed.load()){
		currentKey = LV_KEY_ENTER;
		pressed = true;
	}else{
		const int h = joystick->getHorizontal();
		const int v = joystick->getVertical();

		// The axis pushed further wins. Checking sideways first lost UP whenever the stick
		// drifted a little sideways on the way up (menus only use up and down).
		if(std::abs(v) >= std::abs(h)){
			if(v >= DirThreshold){
				currentKey = LV_KEY_UP;
				pressed = true;
			}else if(v <= -DirThreshold){
				currentKey = LV_KEY_DOWN;
				pressed = true;
			}
		}else{
			if(h >= DirThreshold){
				currentKey = LV_KEY_RIGHT;
				pressed = true;
			}else if(h <= -DirThreshold){
				currentKey = LV_KEY_LEFT;
				pressed = true;
			}
		}
	}

	if(pressed && lastReportedPressed && lastReportedKey != currentKey){
		data->key = lastReportedKey;
		data->state = LV_INDEV_STATE_RELEASED;
		lastReportedPressed = false;
		return;
	}

	if(pressed){
		data->key = currentKey;
		data->state = LV_INDEV_STATE_PRESSED;
		lastReportedKey = currentKey;
		lastReportedPressed = true;
	}else{
		data->key = lastReportedKey;
		data->state = LV_INDEV_STATE_RELEASED;
		lastReportedPressed = false;
	}
}

void JoystickInputLVGL::onButtonInput(Enum<int> btn, ButtonInput::Action action){
	if(btn != enterButton){
		return;
	}

	enterPressed.store(action == ButtonInput::Action::Press);
}

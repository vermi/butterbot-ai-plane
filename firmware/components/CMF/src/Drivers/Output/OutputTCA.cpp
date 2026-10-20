#include "OutputTCA.h"

OutputTCA::OutputTCA(const std::vector<OutputPinDef>& outputs, TCA9555* tca9555) : Super(outputs), tca9555(tca9555){

}

void OutputTCA::performWrite(int port, float value) noexcept{
	if(!getInversions().contains(port)){
		CMF_LOG(CMF, Warning, "Output port %d inversion missing", port);
		return;
	}

	// Between off and on the pin is released to the expander's internal pull-up (about 100 kOhm):
	// an LED driven from it glows faintly, as the status LED does while the power button is held
	// at boot. It is the only dimming an expander pin can do.
	const int8_t level = value <= 0.0f ? 0 : value < 0.5f ? 1 : 2;
	if(port >= 0 && port < (int) shown.size()){
		if(shown[port] == level){
			return;
		}
		shown[port] = level;
	}
	if(level == 1){
		tca9555->pinMode(port, TCA9555::PinMode::IN);
		return;
	}
	tca9555->write(port, level == 2);
	tca9555->pinMode(port, TCA9555::PinMode::OUT);
}

void OutputTCA::performRegister(const OutputPinDef& output) noexcept{
	if(output.port >= 0 && output.port < (int) shown.size()){
		shown[output.port] = -1;
	}
	tca9555->pinMode(output.port, TCA9555::PinMode::OUT);
}

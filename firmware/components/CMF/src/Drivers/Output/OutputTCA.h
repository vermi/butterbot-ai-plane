#ifndef CMF_OUTPUTTCA_H
#define CMF_OUTPUTTCA_H

#include "Object/Class.h"
#include "Drivers/Interface/OutputDriver.h"
#include "Devices/TCA9555.h"
#include <array>

class OutputTCA : public OutputDriver {
	GENERATED_BODY(OutputTCA, OutputDriver, CONSTRUCTOR_PACK(const std::vector<OutputPinDef>&, TCA9555*));

public:
	OutputTCA() = default;
	OutputTCA(const std::vector<OutputPinDef>& outputs, TCA9555* tca9555);

	void performWrite(int port, float value) noexcept override;
	void performRegister(const OutputPinDef& output) noexcept override;

private:
	StrongObjectPtr<TCA9555> tca9555;
	// Level each pin was last set to (-1 unknown, 0 off, 1 dim, 2 on). LED functions are written
	// on every tick of the LED service, so unchanged writes are skipped instead of going over I2C.
	static constexpr std::array<int8_t, 16> unknown(){
		std::array<int8_t, 16> a{};
		a.fill(-1);
		return a;
	}
	std::array<int8_t, 16> shown = unknown();
};


#endif //CMF_OUTPUTTCA_H

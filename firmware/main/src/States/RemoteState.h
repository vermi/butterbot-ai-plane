#ifndef BUTTERBOT_FIRMWARE_REMOTESTATE_H
#define BUTTERBOT_FIRMWARE_REMOTESTATE_H

#include "State.h"
#include "Routines/Routine.h"
#include "Link/LinkService.h"
#include "Services/BaseBoard.h"
#include "Services/ISRButtonInput.h"

/**
 * AI mode: the laptop control plane drives the robot over the link. Autonomy is off.
 * Every drive command carries a duration and the motors stop on their own when it runs out,
 * when the link drops, or when a safety check trips (obstacle ahead, lifted, charging).
 * Leaves on exit_remote, link loss, or a short press of the power button.
 */
class RemoteState : public State {
public:
	explicit RemoteState(BBStateMachine* sm);
	~RemoteState() override;

	void tick(float deltaTime) override;
	int64_t getDynamicTickInterval() const override;

private:
	class RemoteSession* session = nullptr;
	LinkService* link = nullptr;
	BaseBoard* base = nullptr;
	class MotionService* motion = nullptr;
	class Audio* audio = nullptr;

	bool driving = false;
	bool drivingForward = false;
	bool drivingSafe = true;
	bool checkTilt = true;
	bool checkObstacle = true;
	uint64_t driveDeadline = 0;
	ProxState front = ProxState::Unknown;
	ProxState bottom = ProxState::Unknown;

	bool scenarioRunning = false;
	uint32_t scenarioId = 0;
	Routine::TickingState lastTickingState = Routine::TickingState::None;

	uint64_t buttonPressedAt = 0;
	static constexpr uint64_t ShortPressMs = 500;

	// Leave remote mode if the plane is not connected by then (0 = connected).
	uint64_t linkDeadline = 0;
	static constexpr uint64_t InitialConnectMs = 30000; // join Wi-Fi + DHCP + dial (normally 3-10 s)
	static constexpr uint64_t LinkLossGraceMs = 15000;

	bool abortRequested = false; // transitions cannot be requested from the constructor
	bool connectedOnce = false;  // the plane connected at least once in this session
	bool statusLedTouched = false;
	float savedGain = 1.0f;

	void onRequest(LinkRequest req);
	void onConnection(bool online);
	/** Say a short status phrase in the robot's voice; `wait` blocks until it has been said. */
	void speak(const char* text, bool wait);
	void onRadioIP(uint32_t ip);
	void onRadioDisconnected(uint8_t reason, int8_t rssi);
	void onProximityChange(ProxSensor sensor, bool covered);
	void onProximityReading(ProxState f, ProxState b);
	void onButton(int button, ISRButtonInput::Action action);
	void onAudioStatus(bool playing);

	void drive(const LinkRequest& req);
	void stopDriving(const char* reason);
	const char* driveRefusal(bool forward);
	void led(const LinkRequest& req);
	void startScenario(const LinkRequest& req);
	void finishScenario(bool aborted = false);
	void irList(uint32_t id);
	void irSend(const LinkRequest& req);
	void modules(uint32_t id);
};

#endif //BUTTERBOT_FIRMWARE_REMOTESTATE_H

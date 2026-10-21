#ifndef BUTTERBOT_FIRMWARE_REMOTESTATE_H
#define BUTTERBOT_FIRMWARE_REMOTESTATE_H

#include "State.h"
#include "Routines/Routine.h"
#include "Link/LinkService.h"
#include "Services/BaseBoard.h"
#include "Services/ISRButtonInput.h"
#include "Services/LEDRhythm.h"
#include "Phone/Phone.h"
#include "Services/Com.h"
#include <CtrlData.h>
#include <BBData.h>
#include <Scenarios.h>

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
	float savedGain = 1.0f;

	// The status LED (see showLed()): the plane picks the rhythm; the robot shows its own while it
	// talks and while the plane is out of reach.
	LEDRhythm planeLed;
	LEDRhythm shownLed;
	bool ledShown = false;
	bool speaking = false;
	uint64_t speakingUntil = 0; // keep the talking light a moment after the audio stops
	uint64_t enteredAt = 0;

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
	void onPhoneNotif(Phone::NotifEvent event);
	void onPhoneConn(Phone::ConnEvent event);

	// The handheld controller in AI mode: Manual Override hands over to stock RC mode (and comes
	// back); Shut up, Poke, Summon and Listen go to the plane; the menu runs scenarios.
	void onRemoteCommand(Ctrl::Command command);
	void onRemoteRC(RCData data);
	void onRemoteScenario(BB::Action::Scenario scenario, ScenarioData data);
	void onRemoteConn(Com::ConnStatus status);
	/** AI mode's state, as the plane last reported it, for the controller's screen. */
	AIStatusData aiStatus;
	void sendAiStatus();
	// The controller's home screen (see HomeInfo): refreshed with the plane's news and every minute.
	uint64_t homeSentAt = 0;
	static constexpr uint64_t HomeRefreshMs = 60000;
	// The camera on the controller's screen: the plane sends small JPEGs while it is open, and they
	// go out over Bluetooth one piece per tick. A newer picture replaces one still waiting.
	bool peeking = false;
	std::string peekJpeg;   // being sent
	std::string peekNext;   // waiting
	std::string peekCaption;
	uint8_t peekSeq = 0;
	uint8_t peekIndex = 0;
	void peek(bool on);
	void peekFrame(std::string jpeg, const std::string& caption);
	void relayPeek();
	// The controller shows its listening window after Summon's short press, until the agent has
	// the command (or a while has passed).
	bool remoteListening = false;
	uint64_t remoteListenUntil = 0;
	void sendRemote(const std::string& button, const std::string& extra = "");
	/** Tell the controller the robot is idle again: it clears what a scenario put on its screen. */
	void remoteIdle();

	void drive(const LinkRequest& req);
	void stopDriving(const char* reason);
	const char* driveRefusal(bool forward);
	void led(const LinkRequest& req);
	void showLed();
	void startScenario(const LinkRequest& req);
	void finishScenario(bool aborted = false);
	void irList(uint32_t id);
	void irSend(const LinkRequest& req);
	void modules(uint32_t id);
	void phone(const LinkRequest& req);
};

#endif //BUTTERBOT_FIRMWARE_REMOTESTATE_H

#include "RemoteState.h"
#include <Core/Application.h>
#include <Devices/Camera.h>
#include <Services/Audio/Audio.h>
#include <Services/LED/LED.h>
#include <Services/Modules/ModuleService.h>
#include <Services/Modules/ModuleDevices/RM_IRModule.h>
#include <Services/Modules/ModuleDevices/RM_TempHumModule.h>
#include <Services/Modules/ModuleDevices/RM_CO2Sensor.h>
#include <Services/Modules/ModuleDevices/RM_MotionSensor.h>
#include <Scenarios.h>
#include <Util/ServiceLocator.h>
#include <Util/stdafx.h>
#include "BBStateMachine.h"
#include "IdleState.h"
#include "ScenarioRoutineMappings.h"
#include "Enums.h"
#include "Audio/SpeechAudioSource.h"
#include "Services/LEDModuleControl.h"
#include "Services/MotionService.h"
#include "Link/RemoteSession.h"
#include "BLE/ConMan.h"
#include <Periphery/WiFi.h>

DEFINE_LOG(RemoteState)

namespace {
	// Status LED rhythms the robot shows on its own; the plane picks the others (listening,
	// thinking, focus...).
	constexpr LEDRhythm Connecting{ 60, 0, 1, 1000, 0.0f, LEDRhythm::Bright };  // a short tick every second
	constexpr LEDRhythm Trouble{ 80, 120, 3, 1600, 0.0f, LEDRhythm::Bright };   // three quick blinks, pause
	constexpr LEDRhythm Online = LEDRhythm::steady(LEDRhythm::Dim);             // until the plane picks one
	constexpr LEDRhythm Talking = LEDRhythm::steady(LEDRhythm::Bright);
	constexpr uint64_t TroubleAfterMs = 12000; // still connecting after this long: show trouble
	constexpr uint64_t TalkingHoldMs = 400;    // bridges the gaps between spoken sentences
}

RemoteState::RemoteState(BBStateMachine* sm) : State(sm){
	const Application* app = Application::getApp();
	link = app->getService<LinkService>();
	base = app->getService<BaseBoard>();
	motion = app->getService<MotionService>();
	audio = app->getService<Audio>();

	if(link == nullptr || base == nullptr){
		CMF_LOG(RemoteState, LogLevel::Error, "LinkService or BaseBoard missing");
		abortRequested = true; // handled in tick(): the state machine drops transitions requested here
		return;
	}

	if(LEDModuleControl* leds = app->getService<LEDModuleControl>()){
		leds->turnLedOff();
	}
	// The status LED shows AI mode's states from here on, not the stock audio reactions.
	if(ServiceLocator::LEDControlInstance){
		ServiceLocator::LEDControlInstance->suspend(true);
	}
	enteredAt = millis();
	planeLed = Online;
	showLed();

	session = new RemoteSession(link, app->getDevice<Camera>(), audio);

	link->setRemote(true);
	sm->bindState(link->OnRequest, this, &RemoteState::onRequest);
	sm->bindState(link->OnConnection, this, &RemoteState::onConnection);
	sm->bindState(base->onProximityChange, this, &RemoteState::onProximityChange);
	sm->bindState(base->onProximityReading, this, &RemoteState::onProximityReading);
	if(auto* button = app->getService<ISRButtonInput>()){
		sm->bindState(button->OnButtonEvent, this, &RemoteState::onButton);
	}
	if(audio){
		sm->bindState(audio->OnAudioStatusChanged, this, &RemoteState::onAudioStatus);
	}

	if(audio){
		savedGain = audio->getGain();
	}

	base->stopMotors();
	base->requestProximityState();
	link->reportMode("remote");
	// Shake / pick-up / fall detection only runs while enabled (IdleState enables it for itself);
	// LinkService reports the events to the plane.
	if(motion){
		motion->enable();
	}

	// Joining Wi-Fi takes a few seconds; say so right away. Wait for it: speech allocates from
	// the same internal RAM Wi-Fi start-up needs, and a failed allocation reboots (and so powers
	// off) the robot.
	speak("A I mode on.", true);

	// Wi-Fi only ever runs in this state (like RC mode): autonomy is off, so its internal RAM fits.
	// The camera stays as initialised at boot (128x128 RGB565) until the plane asks for a stream;
	// larger sizes need a slower sensor clock than the board's 27 MHz (the plane sends one).
	// Wi-Fi and BLE share one radio. Use the stock low-power BLE mode (slow advertising and
	// connection intervals, ConManager::goLowPow) while the Wi-Fi link is in use; the BLE wake
	// still works, just with more latency.
	ConMan.goLowPow();

	// The radio is started here, on the state machine thread (internal-RAM stack), like RCState.
	if(!link->startRadio()){
		CMF_LOG(RemoteState, LogLevel::Error, "Wi-Fi could not be started, leaving remote mode");
		abortRequested = true;
		return;
	}
	// Wi-Fi events on this thread: destroyState() unbinds them before ~RemoteState deletes the radio.
	if(WiFi* radio = link->radio()){
		sm->bindState(radio->OnStationGotIP, this, &RemoteState::onRadioIP);
		sm->bindState(radio->OnStationDisconnected, this, &RemoteState::onRadioDisconnected);
	}
	// Always (re)assert it: we may be re-entered while a previous session's link is shutting down.
	link->wake();
	if(!link->isOnline()){
		linkDeadline = millis() + InitialConnectMs;
	}
}

RemoteState::~RemoteState(){
	if(motion){
		motion->disable(); // the next state enables it again if it wants it
	}
	ConMan.goHiPow();
	bool wifiDown = true;
	if(base){
		base->stopMotors();
	}
	if(link){
		if(scenarioRunning){
			link->error(scenarioId, "left remote mode");
		}
		link->setRemote(false);
		// Take Wi-Fi fully down before the camera is re-initialised and the idle behaviours
		// start allocating (suspend() lets queued replies such as the exit ack go out first).
		wifiDown = link->suspend(); // socket closed
		link->stopRadio();          // Wi-Fi driver down, its internal RAM free again
	}
	delete session;
	session = nullptr;
	// The camera was released for Wi-Fi; give it back to the stock behaviours now that Wi-Fi's RAM
	// is free (stock settings are pending; init() is a no-op if it is already up).
	if(wifiDown){
		if(Camera* camera = Application::getApp()->getDevice<Camera>()){
			camera->setFormat(PIXFORMAT_RGB565);
			camera->setRes(FRAMESIZE_128X128);
			camera->init();
		}
	}
	if(audio){
		// Volume set by the plane must not leak into the robot's own behaviour.
		audio->setGain(savedGain);
	}
	if(ServiceLocator::LEDControlInstance){
		ServiceLocator::LEDControlInstance->suspend(false);
		ServiceLocator::LEDControlInstance->startIdle();
	}
}

int64_t RemoteState::getDynamicTickInterval() const{
	if(scenarioRunning){
		if(lastTickingState == Routine::TickingState::Continue){
			return 0;
		}
		// Blocking routines wake the state machine through their own events.
		return lastTickingState == Routine::TickingState::Block ? 50 : 20;
	}
	// Fine enough for the drive watchdog.
	return 20;
}

void RemoteState::tick(float deltaTime){
	if(sm->isTransitionPending()){
		return;
	}

	if(link){
		link->persist();
	}
	showLed();

	if(abortRequested || (linkDeadline != 0 && millis() > linkDeadline)){
		// Without this the robot just went back to idle, and nobody could tell a short press had
		// done anything at all.
		if(abortRequested){
			speak("A I mode could not start.", true);
		}else{
			CMF_LOG(RemoteState, LogLevel::Warning, "no plane connection, leaving remote mode");
			speak(connectedOnce ? "Lost the computer." : "I can't reach the computer.", true);
		}
		sm->transitionTo<IdleState>();
		return;
	}

	if(scenarioRunning){
		Routine* routine = sm->getActiveRoutine();
		lastTickingState = routine ? routine->tick(deltaTime) : Routine::TickingState::Done;
		if(lastTickingState == Routine::TickingState::Done){
			finishScenario();
		}
		return;
	}

	if(driving && millis() >= driveDeadline){
		stopDriving("watchdog");
	}
}

// ---------------------------------------------------------------- requests

void RemoteState::onRequest(LinkRequest req){
	using T = LinkRequest::Type;

	if(scenarioRunning && req.type != T::ExitRemote && req.type != T::Stop){
		if(req.id != 0){
			link->error(req.id, "a scenario is running");
		}
		return;
	}

	switch(req.type){
		case T::EnterRemote:
			link->ack(req.id);
			break;

		case T::ExitRemote:
			link->ack(req.id);
			// Speaking blocks this thread (no watchdog, no obstacle stop): stop moving first.
			base->stopMotors();
			driving = false;
			speak("A I mode off.", true);
			sm->transitionTo<IdleState>();
			break;

		case T::Drive:
			drive(req);
			break;

		case T::Stop:
			if(scenarioRunning){
				sm->endRoutine();
				finishScenario(true);
			}
			if(driving){
				base->stopMotors();
				driving = false;
			}
			break;

		case T::Say:
			if(audio && ServiceLocator::SpeechAudioGenInstance && !req.text.empty()){
				session->speakerAbort();
				// flite renders the whole utterance; keep it bounded.
				audio->play(ServiceLocator::SpeechAudioGenInstance.get(), std::make_unique<SpeechAudioSource>(SpeechGen::InputType::Text, req.text.substr(0, 2000)));
				link->ack(req.id);
			}else{
				link->error(req.id, "speech unavailable");
			}
			break;

		case T::SpkBegin:
			if(audio == nullptr){
				link->error(req.id, "audio unavailable");
				break;
			}
			session->speakerBegin();
			link->ack(req.id);
			break;

		case T::SpkEnd:
			session->speakerEnd();
			break;

		case T::SpkAbort:
			session->speakerAbort();
			if(audio){
				audio->stop();
			}
			break;

		case T::Volume:
			if(audio){
				audio->setGain(req.gain);
			}
			break;

		case T::Camera:
			session->setCamera(req.on, req.res, req.fps, req.quality, req.swJpeg, req.xclkMhz, req.psramDma);
			link->ack(req.id);
			break;

		case T::Snapshot:
			session->requestSnapshot(req.id, req.res, req.quality); // acked with the frame
			break;

		case T::Mic:
			session->setMic(req.on, req.gate, req.gain);
			link->ack(req.id);
			break;

		case T::Led:
			led(req);
			break;

		case T::StatusLight:
			planeLed = req.rhythm;
			break;

		case T::Scenario:
			startScenario(req);
			break;

		case T::IrList:
			irList(req.id);
			break;

		case T::IrSend:
			irSend(req);
			break;

		case T::Modules:
			modules(req.id);
			break;

		case T::Safety:
			checkTilt = req.checkTilt;
			checkObstacle = req.checkObstacle;
			link->ack(req.id);
			break;
	}
}

// ---------------------------------------------------------------- driving

// Same checks as the stock movement routines (WanderRoutine, MovementCommandRoutine) make when a
// movement starts: upright and on the ground, and nothing in front when going forward. Driving
// while charging is allowed on purpose (the AI build is often tested on USB).
const char* RemoteState::driveRefusal(bool forward){
	// Accelerometer only: the bottom IR sensor misreads around motor activity (and its "lifted"
	// flag latches), which refused every move after the first one.
	if(checkTilt && motion && motion->isTipped()){
		return "tipped_over";
	}
	if(checkObstacle && forward && front == ProxState::Covered){
		return "obstacle";
	}
	return nullptr;
}

void RemoteState::drive(const LinkRequest& req){
	if(req.left == 0 && req.right == 0){
		if(driving){
			base->stopMotors();
			driving = false;
		}
		return;
	}

	// Direct control (safe=false) behaves exactly like the stock RC mode: no checks at all.
	// AI drives get the stock autonomous-move checks, made when a movement starts (or turns forward)
	// like stock; the plane refreshes a drive every 250 ms and re-checking would trip on motor noise.
	const bool forward = req.left + req.right > 0;
	if(req.safe && (!driving || !drivingSafe || (forward && !drivingForward))){
		if(const char* reason = driveRefusal(forward)){
			stopDriving(reason);
			return;
		}
	}

	base->setMotors(req.left, req.right);
	driving = true;
	drivingForward = forward;
	drivingSafe = req.safe;
	driveDeadline = millis() + std::min<uint16_t>(req.ms, 5000);
}

void RemoteState::stopDriving(const char* reason){
	base->stopMotors();
	driving = false;
	link->sendJson(std::string("{\"t\":\"drive_stopped\",\"reason\":\"") + reason + "\"}");
}

void RemoteState::onProximityReading(ProxState f, ProxState b){
	front = f;
	bottom = b;
}

void RemoteState::onProximityChange(ProxSensor sensor, bool covered){
	if(sensor == ProxSensor::Front){
		front = covered ? ProxState::Covered : ProxState::Uncovered;
		if(checkObstacle && covered && driving && drivingForward && drivingSafe){
			stopDriving("obstacle");
		}
	}else{
		// Stock movement routines ignore the bottom sensor while moving; so do we.
		bottom = covered ? ProxState::Covered : ProxState::Uncovered;
	}
}

void RemoteState::onRadioIP(uint32_t ip){
	link->onGotIP(ip);
}

void RemoteState::onRadioDisconnected(uint8_t reason, int8_t rssi){
	link->onStaDisconnected(reason);
}

void RemoteState::onConnection(bool online){
	if(online){
		linkDeadline = 0;
		link->radioConfirmed();
		connectedOnce = true;
		// BaseBoard calls are made from this thread only (they abort on I2C errors).
		base->requestProximityState();
		return;
	}
	// Never keep driving without the plane; give it a moment to reconnect before leaving.
	base->stopMotors();
	driving = false;
	if(session){
		session->speakerAbort();
	}
	linkDeadline = millis() + LinkLossGraceMs;
}

void RemoteState::onButton(int button, ISRButtonInput::Action action){
	if(button != static_cast<int>(Button::Power)){
		return;
	}
	if(action == ISRButtonInput::Action::Press){
		buttonPressedAt = millis();
		return;
	}
	// Short press hands control back to the robot; long presses are the shutdown gesture.
	if(buttonPressedAt != 0 && millis() - buttonPressedAt <= ShortPressMs){
		base->stopMotors();
		speak("A I mode off.", true);
		sm->transitionTo<IdleState>();
	}
	buttonPressedAt = 0;
}

void RemoteState::speak(const char* text, bool wait){
	if(audio == nullptr || !ServiceLocator::SpeechAudioGenInstance){
		return;
	}
	if(session){
		session->speakerAbort();
	}
	// This may block the thread that updates the LED; light it now (the audio events take over).
	speakingUntil = millis() + 3000;
	showLed();
	audio->play(ServiceLocator::SpeechAudioGenInstance.get(), std::make_unique<SpeechAudioSource>(SpeechGen::InputType::Text, text));
	if(wait){
		audio->waitEnd(pdMS_TO_TICKS(5000));
	}
}

void RemoteState::onAudioStatus(bool playing){
	speaking = playing;
	if(!playing){
		speakingUntil = millis() + TalkingHoldMs;
	}
	link->sendJson(std::string("{\"t\":\"speaker\",\"playing\":") + (playing ? "true" : "false") +
				   ",\"underruns\":" + std::to_string(session ? session->speakerUnderruns() : 0) + "}");
}

// ---------------------------------------------------------------- LEDs, scenarios, IR, modules

void RemoteState::led(const LinkRequest& req){
	const Application* app = Application::getApp();

	if(req.ledTarget == 1){
		auto* modules = app->getService<ModuleService>();
		auto* leds = app->getService<LEDModuleControl>();
		if(modules == nullptr || leds == nullptr || modules->getInserted() != Modules::Type::RM_LED){
			link->error(req.id, "no LED module inserted");
			return;
		}
		switch(req.ledMode){
			case 1: leds->turnLedOff(); break;
			case 2: leds->turnLedStrobe(); break;
			case 3: leds->turnLedBreathe(); break;
			default: leds->turnLedOn(); break;
		}
		link->ack(req.id);
		return;
	}

	// The status LED is off, dim (level below 0.5) or bright; it cannot fade, so "breathe" swaps
	// dim and the level slowly. Shown by showLed() unless the robot is talking or the plane is out
	// of reach.
	switch(req.ledMode){
		case 1:
			planeLed = LEDRhythm::steady(0.0f);
			break;
		case 2:
			planeLed = { 250, 0, 1, 500, 0.0f, req.level };
			break;
		case 3:
			planeLed = { 1000, 0, 1, 2000, LEDRhythm::Dim, req.level };
			break;
		default:
			planeLed = LEDRhythm::steady(req.level);
			break;
	}
	link->ack(req.id);
}

void RemoteState::showLed(){
	if(link == nullptr){
		return;
	}
	LEDRhythm want = planeLed;
	if(speaking || millis() < speakingUntil){
		want = Talking;
	}else if(!link->isOnline()){
		want = connectedOnce || millis() - enteredAt > TroubleAfterMs ? Trouble : Connecting;
	}
	if(ledShown && want == shownLed){
		return;
	}
	if(ServiceLocator::LEDControlInstance && ServiceLocator::LEDControlInstance->isBatteryCritical()){
		return;
	}
	auto* ledService = Application::getApp()->getService<LED<MonoLED, RGBLED>>();
	if(ledService == nullptr){
		return;
	}
	ledService->set(MonoLED::Status, std::make_unique<LEDRhythmFunction<MonoLED>>(want));
	shownLed = want;
	ledShown = true;
}

void RemoteState::startScenario(const LinkRequest& req){
	// These load on-robot speech or vision models. Their internal RAM needs were never budgeted
	// next to Wi-Fi (stock runs them without it), and an allocation failure aborts the firmware.
	using S = BB::Action::Scenario;
	static constexpr S NeedModels[] = {
		S::EightBall, S::DiceRoll, S::VoiceControl, S::Dance, S::FaceDetect, S::FaceDetectForget,
		S::IR_train, S::IR_forget, S::WhatsThis,
	};
	for(const S s : NeedModels){
		if(static_cast<uint32_t>(s) == req.scenario){
			link->error(req.id, "this scenario needs on-robot speech/vision models and is not available in AI mode");
			return;
		}
	}

	ScenarioData data{};
	data.raw = req.arg;
	const RoutineFactory factory = routineForScenario({ static_cast<BB::Action::Scenario>(req.scenario), data });
	if(factory == nullptr){
		link->error(req.id, "no routine for this scenario");
		return;
	}

	base->stopMotors();
	driving = false;
	session->speakerAbort();
	// Stock routines own the camera (128x128 RGB565) and the mic (AudioFrontend) while they run.
	// If the camera cannot be re-initialised next to Wi-Fi it stays off: none of the scenarios
	// allowed in AI mode use it (the camera ones are refused above).
	session->pause(true);

	scenarioId = req.id;
	scenarioRunning = true;
	lastTickingState = Routine::TickingState::None;
	sm->startRoutine(factory);
}

void RemoteState::finishScenario(bool aborted){
	if(sm->getActiveRoutine() != nullptr){
		sm->endRoutine();
	}
	scenarioRunning = false;
	lastTickingState = Routine::TickingState::None;
	base->stopMotors();
	session->pause(false);
	ledShown = false; // routines may have used the LED; show ours again
	link->ack(scenarioId, aborted ? "{\"done\":false,\"aborted\":true}" : "{\"done\":true}");
}

void RemoteState::irList(uint32_t id){
	if(!ServiceLocator::IRStorageInstance){
		link->error(id, "IR storage unavailable");
		return;
	}
	std::string json = "{\"commands\":[";
	const uint8_t count = ServiceLocator::IRStorageInstance->getCount();
	for(uint8_t i = 0; i < count; i++){
		const IRCommand& cmd = ServiceLocator::IRStorageInstance->getCommand(i);
		if(i > 0){
			json += ",";
		}
		json += "{\"index\":" + std::to_string(i) + ",\"phonemes\":\"" + LinkService::escape(std::string(cmd.phonemes, strnlen(cmd.phonemes, sizeof(cmd.phonemes)))) + "\"}";
	}
	json += "]}";
	link->ack(id, json);
}

void RemoteState::irSend(const LinkRequest& req){
	auto* modules = Application::getApp()->getService<ModuleService>();
	if(modules == nullptr || modules->getInserted() != Modules::Type::RM_IR){
		link->error(req.id, "no IR module inserted");
		return;
	}
	auto* ir = cast<RM_IRModule>(modules->getDevice().get());
	if(ir == nullptr || !ServiceLocator::IRStorageInstance || req.index >= ServiceLocator::IRStorageInstance->getCount()){
		link->error(req.id, "no such IR command");
		return;
	}
	const IRCommand& cmd = ServiceLocator::IRStorageInstance->getCommand(req.index);
	ir->sendIR(cmd.frame, cmd.frameCount, cmd.sendCount, cmd.periodUs, cmd.carrierHz, cmd.carrierDutyPct);
	link->ack(req.id);
}

void RemoteState::modules(uint32_t id){
	auto* modules = Application::getApp()->getService<ModuleService>();
	if(modules == nullptr){
		link->error(id, "module service unavailable");
		return;
	}

	const Modules::Type type = modules->getInserted();
	auto device = modules->getDevice();
	std::string json = "{\"modules\":[";
	switch(type){
		case Modules::Type::RM_TempHum:
			if(auto* th = cast<RM_TempHumModule>(device.get())){
				th->sample();
				json += "{\"bus\":0,\"type\":\"temp_hum\",\"temperature_c\":" + std::to_string(th->getTemperature()) +
						",\"humidity\":" + std::to_string(th->getHumidity()) + "}";
			}
			break;
		case Modules::Type::RM_CO2:
			if(auto* co2 = cast<RM_CO2Sensor>(device.get())){
				json += "{\"bus\":0,\"type\":\"co2\",\"reading\":" + std::to_string(co2->getReading()) + ",\"ok\":" + (co2->isOK() ? "true" : "false") + "}";
			}
			break;
		case Modules::Type::RM_Motion:
			if(auto* pir = cast<RM_MotionSensor>(device.get())){
				json += std::string("{\"bus\":0,\"type\":\"motion\",\"detected\":") + (pir->getState() ? "true" : "false") + "}";
			}
			break;
		case Modules::Type::RM_LED:
			json += "{\"bus\":0,\"type\":\"led\"}";
			break;
		case Modules::Type::RM_IR:
			json += "{\"bus\":0,\"type\":\"ir\"}";
			break;
		case Modules::Type::RM_PerfBoard:
			json += "{\"bus\":0,\"type\":\"perfboard\"}";
			break;
		default:
			break;
	}
	json += "]}";
	link->ack(id, json);
}

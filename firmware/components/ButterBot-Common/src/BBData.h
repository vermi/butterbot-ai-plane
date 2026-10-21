#ifndef BUTTERBOT_COMMON_ACTIONDATA_H
#define BUTTERBOT_COMMON_ACTIONDATA_H

#include <ctime>
#include <cstddef>
#include <cstdint>
#include <string>
#include <vector>
#include "Phrases.h"
#include "Serialization.h"

static constexpr const char* ControllerIP = "11.0.0.1";
static constexpr const char* ButterbotIP = "11.0.0.2";
static constexpr uint16_t UDPPort = 6001;

struct BB {
	enum class State {
		Idle, Listen, Scenario, RC,
		AI, // AI mode (the laptop control plane): its status for the controller
	} state;

	union Action {
		enum class Idle {
			None, Wander, Ramble, Person, Poke, Summon, Fall, UpsideDown, PickUp, Shake, ModuleChange, Gas, Intruder,
			BatteryLow, Charging, ChargingFull, PhoneNotif, PhoneConnect, Observe, GasConfigureStart, GasConfigureEnd,
			BatteryLevel, ShutUp, CantMove,
			Home, // the controller's home screen (HomeInfoData): telemetry like BatteryLevel, not an action
		} idle;

		enum class Scenario {
			ReadNotifs, EightBall, Fact, Joke, PassTheButter, Profanity, YouPassButter, DiceRoll, CurrentTime,
			VoiceControl, Dance,
			FaceDetect, FaceDetectForget,
			IR_train, IR_forget, IR_forgetAll, IR_list, IR_action,
			LEDModule, LEDTurnOn, LEDTurnOff, LEDStrobe, LEDBreathe, LEDFaster, LEDSlower,
			GasModule,
			TempHumModule, TempHumScaleCelsius, TempHumScaleFahrenheit, TempHumScaleKelvin,
			IntruderDetectionOn, IntruderDetectionOff,
			Shutdown,
			PhoneNotConnected,
			PhoneListNotifs,
			PhoneWhatsPlaying,
			PhoneNextSong,
			PhonePrevSong,
			PhonePlayMusic,
			PhoneStopMusic,
			WhatsThis,
			// AI mode only (the controller lists them then); the laptop's agent handles them.
			AiSee, AiNews, AiWeather, AiExplore, AiStory, AiFocus, AiActivity,
			AiPeek, // the controller shows the camera (ScenarioData 1) or stops (0)
		} scenario;

		enum class Listen {
		} listen;

		enum class RC {
			FeedStatus
		} remoteControl;

		enum class AI {
			Status,
			PeekWiFi,    // how the controller reaches the plane for the camera (PeekWiFiData)
			PeekCaption, // what is in the picture (PeekCaptionData)
		} ai;
	} action;

	size_t dataSize;
};

struct BBData {}; // Empty struct. All action-related data structs must inherit this

struct ListenData : BBData {
	enum class Phase : uint8_t {
		Listening, NotUnderstand, Abort
	} phase;
};

/** AI mode's state for the controller (BB::State::AI, BB::Action::AI::Status). */
struct AIStatusData : BBData {
	enum class Mode : uint8_t {
		Off, Listening, Attentive, Thinking, Focus, NoBrain
	} mode = Mode::Off;
	enum class Activity : uint8_t {
		Quiet, Calm, Normal, Lively
	} activity = Activity::Normal;
	uint8_t focusMinutes = 0; // left in focus mode (255: more)
	bool speaking = false;    // the robot is talking (it sets this itself)
	uint8_t emotion = 0;      // its mood, in the plane's order: content, happy, excited, curious,
	                          // playful, calm, sleepy, bored, sad, grumpy, surprised, worried
	uint8_t intensity = 0;    // 0-100
};

/**
 * The camera on the controller's screen in AI mode (Scenario::AiPeek). Bluetooth is far too slow
 * for video, so the controller joins the home Wi-Fi next to its own access point and the plane
 * streams to it directly, in the RC feed's format (FeedFrame). The robot hands over what it needs
 * when the view opens; the controller keeps it in memory only while the view is open.
 */
struct PeekWiFiData : BBData {
	static constexpr uint16_t PlanePort = 7533; // the controller says hello here; frames come back to UDPPort

	char ssid[33];
	char password[65];
	uint32_t planeIp; // network byte order
};

struct PeekCaptionData : BBData {
	static constexpr uint8_t MaxText = 64;

	std::string text;

	std::vector<uint8_t> serialize() const{
		std::vector<uint8_t> buf;
		ComSerial::writeString(buf, text, MaxText);
		return buf;
	}

	static PeekCaptionData deserialize(const uint8_t* data, size_t size){
		PeekCaptionData d;
		const uint8_t* cur = data;
		d.text = ComSerial::readString(cur, data + size);
		return d;
	}
};

/**
 * The controller's home screen (BB::State::Idle, BB::Action::Idle::Home): the robot's battery and
 * clock, and in AI mode the agent's latest news. Sent on connect, on battery changes, every
 * minute in AI mode and whenever the plane has news. The controller keeps the clock running.
 */
struct HomeInfoData : BBData {
	static constexpr uint8_t MaxNext = 48;
	static constexpr uint8_t MaxSeen = 32;
	static constexpr uint8_t MaxSaid = 100;
	static constexpr uint8_t MaxPlaying = 64;
	static constexpr uint8_t MaxHeard = 80;
	static constexpr uint8_t MaxDoing = 24;

	uint8_t botPercent = 0;
	bool clockSet = false;
	// Local time; month 1-12, weekday 0 = Sunday.
	uint16_t year = 0;
	uint8_t month = 0, day = 0, weekday = 0, hour = 0, minute = 0, second = 0;
	// AI mode only (empty otherwise).
	std::string next;      // the next reminder, "15:00 Call mom"
	std::string seen;      // the latest thing noticed, "Saw Kiwi"
	uint16_t seenSecs = 0; // how long ago
	std::string said;      // the last thing it said
	std::string playing;   // the phone's music, "Title - Artist" (any mode; empty unless playing)
	std::string heard;     // AI mode: the last thing someone said to it
	bool heardLast = false; // that came after `said`
	std::string doing;     // AI mode: what it is up to right now ("dancing"), if anything

	std::vector<uint8_t> serialize() const{
		std::vector<uint8_t> buf;
		ComSerial::writeU8(buf, botPercent);
		ComSerial::writeU8(buf, clockSet);
		ComSerial::writeU8(buf, year & 0xff);
		ComSerial::writeU8(buf, year >> 8);
		for(uint8_t v : { month, day, weekday, hour, minute, second }){
			ComSerial::writeU8(buf, v);
		}
		ComSerial::writeString(buf, next, MaxNext);
		ComSerial::writeString(buf, seen, MaxSeen);
		ComSerial::writeU8(buf, seenSecs & 0xff);
		ComSerial::writeU8(buf, seenSecs >> 8);
		ComSerial::writeString(buf, said, MaxSaid);
		ComSerial::writeString(buf, playing, MaxPlaying);
		ComSerial::writeString(buf, heard, MaxHeard);
		ComSerial::writeU8(buf, heardLast);
		ComSerial::writeString(buf, doing, MaxDoing);
		return buf;
	}

	static HomeInfoData deserialize(const uint8_t* data, size_t size){
		HomeInfoData d;
		const uint8_t* cur = data;
		const uint8_t* end = data + size;
		d.botPercent = ComSerial::readU8(cur, end);
		d.clockSet = ComSerial::readU8(cur, end) != 0;
		d.year = ComSerial::readU8(cur, end);
		d.year |= ComSerial::readU8(cur, end) << 8;
		for(uint8_t* v : { &d.month, &d.day, &d.weekday, &d.hour, &d.minute, &d.second }){
			*v = ComSerial::readU8(cur, end);
		}
		d.next = ComSerial::readString(cur, end);
		d.seen = ComSerial::readString(cur, end);
		d.seenSecs = ComSerial::readU8(cur, end);
		d.seenSecs |= ComSerial::readU8(cur, end) << 8;
		d.said = ComSerial::readString(cur, end);
		d.playing = ComSerial::readString(cur, end);
		d.heard = ComSerial::readString(cur, end);
		d.heardLast = ComSerial::readU8(cur, end) != 0;
		d.doing = ComSerial::readString(cur, end);
		return d;
	}
};

struct RambleData : BBData {
	uint8_t id;
};

struct WanderData : BBData {};

struct PersonData : BBData {
	uint8_t id;
	bool greetKnown;
};

struct FactData : BBData {
	uint8_t id;
};

struct JokeData : BBData {
	uint8_t id;
};

struct PassTheButterData : BBData {
	uint8_t id;
};

struct ProfanityData : BBData {
	uint8_t id;
};

struct YouPassButterData : BBData {
	uint8_t id;
};

struct PokeData : BBData {
	enum class Level : uint8_t {
		One, Two, Three, COUNT
	} level;

	uint8_t id;
};

struct ShutUpData : BBData {
	bool muted;
};

struct SummonData : BBData {
	enum class Phase : uint8_t {
		Start, Found, NotFound, Charging, Unsafe, Interrupted
	} phase;

	uint8_t id;
};

struct FallData : BBData {};

struct UpsideDownData : BBData {};

struct PickUpData : BBData {};

struct ShakeData : BBData {};

struct EightBallData : BBData {
	enum class Phase : uint8_t {
		Listening, Thinking, Response
	} phase;

	uint8_t responseId;
};

struct IR_trainData : BBData {
	enum class State : uint8_t {
		MaxCapacity, Scanning, ScanDone, Listening, SaveDone
	} state;

	uint8_t id;
	bool missing;
};

struct IR_forgetData : BBData {
	enum class State : uint8_t {
		Listening, NotRecognised, Done, Empty
	} state;
};

struct IR_forgetAllData : BBData {
	enum class State : uint8_t {
		Done, Empty
	} state;
};

struct IR_listData : BBData {
	enum class State : uint8_t {
		NoActions, ListingActions
	} state;
};

struct IR_actionData : BBData {
    uint8_t commandIndex;
};

struct GasData : BBData {
	uint8_t id;
	bool missing;
	bool ok;
};

struct IntruderData : BBData {
	uint8_t id;
	bool detected;
};

enum class ModuleType : uint8_t {
	Unknown = 0,
	TempHumidity,
	LED,
	Motion,
	CO2,
	IR,
	PerfBoard,
	COUNT
};

struct ModuleData : BBData {
	uint8_t id;
	ModuleType type;
	bool inserted;
};

struct BatteryData : BBData {
	uint8_t id;

	enum class State : uint8_t {
		Critical, Low, Charging, ChargingFull
	} state;
};

// Mirrors firmware ChargingState (BaseBoard.h). Order must match.
enum class ChargeStatus : uint8_t {
	Unplugged, Charging, Full
};

struct BatteryLevelData : BBData {
	uint8_t level;        // Battery::Level: 0 = Critical .. 6 = Full
	ChargeStatus charge;
};

struct LEDModuleData : BBData {
	uint8_t id;
	bool missing;
};

struct LEDTurnOnData : BBData {
	uint8_t id;
	bool missing;
};

struct LEDTurnOffData : BBData {
	uint8_t id;
	bool missing;
};

struct LEDStrobeData : BBData {
	uint8_t id;
	bool missing;
};

struct LEDBreatheData : BBData {
	uint8_t id;
	bool missing;
};

struct LEDFasterData : BBData {
	uint8_t id;
	bool missing;
};

struct LEDSlowerData : BBData {
	uint8_t id;
	bool missing;
};

struct DiceRollData : BBData {
	enum class Phase : uint8_t {
		RollCount, DiceKind, RollAnim, Result
	} phase;

	enum class DiceType : uint8_t {
		D4, D6, D8, D10, D12, D20, D100, None
	} diceType;

	uint8_t diceCount;
	uint16_t diceResult;
};

struct CurrentTimeData : BBData {
	enum class Status : uint8_t {
		NoTime, ShowTime
	} status;

	tm currentTime;
};

struct VoiceControlData : BBData {
	enum class Direction : uint8_t {
		Forward, Backward, Left, Right, Rotate
	} direction;
};

struct DanceData : BBData {};

enum class TempHumScale : uint8_t {
	Celsius,
	Fahrenheit,
	Kelvin
};

struct TempHumModuleData : BBData {
	uint8_t id;
	bool missing;
	int16_t temperature;
	uint8_t humidity;
	TempHumScale scale;
};

struct TempHumScaleData : BBData {
	uint8_t id;
	TempHumScale scale;
};

struct ShutdownData : BBData {
	uint8_t id;

	enum class ShutdownReason : uint8_t {
		Command,
		Battery
	} reason;
};

struct PIRModuleData : BBData {
	uint8_t id;
	bool missing;
};

struct PhoneConnData : BBData {
	uint8_t id;
	bool connected;
};

struct PhoneNotConnectedData : BBData {
	uint8_t id;
};

struct PhoneListNotifsData : BBData {
	enum class Phase : uint8_t {
		NoNotifs, Count, AskRead, Reading, AskContinue, Done
	} phase;
	uint8_t id;
	uint8_t count;
	uint8_t remaining;
};

struct WhatsPlayingData : BBData {
	static constexpr uint8_t MaxField = 64;

	uint8_t id;
	std::string title;
	std::string artist;
	std::string album;

	std::vector<uint8_t> serialize() const{
		std::vector<uint8_t> buf;
		ComSerial::writeU8(buf, id);
		ComSerial::writeString(buf, title, MaxField);
		ComSerial::writeString(buf, artist, MaxField);
		ComSerial::writeString(buf, album, MaxField);
		return buf;
	}

	static WhatsPlayingData deserialize(const uint8_t* data, size_t size){
		WhatsPlayingData d;
		const uint8_t* cur = data;
		const uint8_t* end = data + size;
		d.id = ComSerial::readU8(cur, end);
		d.title = ComSerial::readString(cur, end);
		d.artist = ComSerial::readString(cur, end);
		d.album = ComSerial::readString(cur, end);
		return d;
	}
};

struct MediaControlData : BBData {
	uint8_t id;
};

struct FeedStatusData : BBData {
	bool status;
};

struct FaceDetectData : BBData {
	enum class State : uint8_t {
		Scanning, Recognized, NotRecognized, NoFaceFound, AlreadySaved, NewOwner
	} state;
};

struct FaceForgetData : BBData {
	enum class State : uint8_t {
		NoDataExist, Done
	} state;
};

struct WhatsThisData : BBData {
	Phrase phrase;
	uint8_t id;
	uint8_t count;
	uint8_t class1;
	uint8_t class2;
};

struct ObserveData : BBData {
	Phrase phrase;
	uint8_t id;
	uint8_t count;
	uint8_t class1;
	uint8_t class2;
};

struct GasConfigureStartData : BBData {
	uint8_t id;
};

struct GasConfigureEndData : BBData {
	uint8_t id;
};

struct CantMoveData : BBData {
	enum class Reason : uint8_t {
		Charging, Unstable
	} reason;
};

#endif //BUTTERBOT_COMMON_ACTIONDATA_H

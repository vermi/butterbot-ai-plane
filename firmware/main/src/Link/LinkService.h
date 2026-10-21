#ifndef BUTTERBOT_FIRMWARE_LINKSERVICE_H
#define BUTTERBOT_FIRMWARE_LINKSERVICE_H

#include <atomic>
#include <array>
#include <lwip/sockets.h>
#include <functional>
#include <mutex>
#include <string>
#include <freertos/FreeRTOS.h>
#include <freertos/queue.h>
#include <freertos/semphr.h>
#include <esp_camera.h>
#include <BBData.h>
#include <CtrlData.h>
#include "Entity/AsyncEntity.h"
#include "Event/EventBroadcaster.h"
#include "Memory/SmartPtr/StrongObjectPtr.h"
#include "Thread/Threaded.h"
#include "LinkProto.h"
#include "Services/BaseBoard.h"
#include "Services/LEDRhythm.h"
#include "Services/MotionService.h"
#include "Services/ISRButtonInput.h"
#include "Battery/Battery.h"

/**
 * A control message from the plane that needs the state machine (see RemoteState).
 * Flat on purpose: events are copied into the receiver's queue.
 */
struct LinkRequest {
	enum class Type : uint8_t {
		EnterRemote, ExitRemote, Drive, Stop, Say, SpkBegin, SpkEnd, SpkAbort, Volume,
		Camera, Snapshot, Mic, Led, StatusLight, Phone, AiStatus, HomeInfo, PeekCaption, Scenario, IrList, IrSend, Modules, Safety
	} type = Type::Stop;

	uint32_t id = 0;
	int8_t left = 0;
	int8_t right = 0;
	uint16_t ms = 0;
	bool on = false;
	bool gate = true;
	float gain = 1.0f;
	float level = 1.0f;
	framesize_t res = FRAMESIZE_QVGA;
	uint8_t fps = 10;
	uint8_t quality = 12;
	// Camera capture strategy: encode RGB565 on the CPU (stock RC feed path) or use the sensor's
	// JPEG output; sensor clock in MHz (0 = board default); PSRAM DMA (-1 = driver default).
	bool safe = true;          // Drive: apply safety checks (AI); false = direct control like the RC controller
	bool checkTilt = true;     // Safety: refuse to start moving when tipped over
	bool checkObstacle = true; // Safety: no forward driving into a covered front sensor
	bool swJpeg = true;
	uint8_t xclkMhz = 0;
	int8_t psramDma = -1;
	uint8_t ledTarget = 0; // 0 status, 1 module
	uint8_t ledMode = 0;   // 0 on, 1 off, 2 blink, 3 breathe
	LEDRhythm rhythm;      // StatusLight
	AIStatusData ai;       // AiStatus: for the handheld controller
	HomeInfoData home;     // HomeInfo: the plane's clock and news for the controller's home screen
	uint32_t scenario = 0;
	uint8_t arg = 0;
	uint8_t index = 0;
	std::string text;      // Say: what to say; Phone: the action; PeekCaption: what is in view
};

/**
 * Link to the AI control plane on the laptop: joins the home Wi-Fi (DHCP), dials the plane over
 * TCP, parses framed messages and sends telemetry, camera frames and microphone audio.
 *
 * Runs independently of the state machine. Control requests that act on the robot are broadcast
 * as LinkRequest events and handled by IdleState (enter_remote) and RemoteState (everything else).
 * Speaker PCM bypasses the event system and goes straight to the registered sink.
 */
class LinkService : public AsyncEntity {
	GENERATED_BODY(LinkService, AsyncEntity, void)

public:
	LinkService();
	~LinkService() override;

	void postInitProperties() noexcept override;

	static LinkService* instance(){ return inst; }

	DECLARE_EVENT(RequestEvent, LinkService, LinkRequest);
	RequestEvent OnRequest{ this };

	/**
	 * bool online - plane connection established (true) or lost (false)
	 */
	DECLARE_EVENT(ConnectionEvent, LinkService, bool);
	ConnectionEvent OnConnection{ this };

	/** BLE wake: IdleState switches to RemoteState, which then calls wake(). */
	DECLARE_EVENT(WakeRequestEvent, LinkService);
	WakeRequestEvent OnWakeRequest{ this };

	/** Store new Wi-Fi/plane settings and reconnect. Safe from any thread. */
	void configure(const LinkConfigData& config);

	/**
	 * Ask for AI mode (BLE wake / provisioning). Returns 1 if requested, 0 if no configuration is
	 * stored, 2 if the robot is busy (not idle). Safe from any thread.
	 */
	uint8_t requestWake();
	/** Dial the plane over the radio started by startRadio(). Only RemoteState may call this. */
	bool wake();

	/**
	 * Wi-Fi radio. State machine thread only (needs an internal-RAM stack, like RCState's WiFi).
	 * startRadio() refuses when internal RAM is short; returns false then.
	 */
	bool startRadio();
	void stopRadio();
	/** The link came up: clear the Wi-Fi crash latch. State machine thread only. */
	void radioConfirmed();
	/** Write pending configuration / tuning to NVS. State machine thread only. */
	void persist();
	/** The radio while it is up (else nullptr). RemoteState binds its events. SM thread only. */
	class WiFi* radio();
	/** Wi-Fi events, forwarded by RemoteState from the state machine thread. */
	void onGotIP(uint32_t ip);
	void onStaDisconnected(uint8_t reason);
	/** Drop the link and turn Wi-Fi off (leaving remote mode). Safe from any thread. */
	void sleep();
	bool isWanted() const { return wanted; }

	/** Turn Wi-Fi off and wait until it is down, so RC mode can use the radio. False on timeout. */
	bool suspend();

	/** RAM thresholds for starting Wi-Fi; stored by the link thread. Zero keeps a value. */
	void tune(const LinkTuneData& data);

	bool isOnline() const { return online; }
	/** The plane's IPv4 address (network byte order), 0 before it is known. */
	uint32_t planeIp() const { return target.sin_addr.s_addr; }

	void setRemote(bool remote){ this->remote = remote; }
	bool isRemote() const { return remote; }
	/** Manual Override left AI mode for stock RC mode: go back to AI mode when RC mode ends. */
	void setResumeAfterRC(bool on){ resumeAfterRC = on; }
	bool takeResumeAfterRC(){ return resumeAfterRC.exchange(false); }
	/** Enter AI mode as soon as the robot is idle (after a restart into AI mode). */
	void setAiOnIdle(bool on){ aiOnIdle = on; }
	bool takeAiOnIdle(){ return aiOnIdle.exchange(false); }
	/** Why startRadio() last failed, for the robot to say: "settings", "memory" or "wifi". */
	const char* startFailure() const { return lastStartFailure; }
	/** Shut up stopped the robot mid-sentence (noted on the app thread, used on the state machine's). */
	void noteHush(){ hushedAt = millis(); }
	bool hushedWithin(uint64_t ms) const { const uint64_t at = hushedAt; return at != 0 && millis() - at < ms; }

	/** Current state machine mode: idle, listen, scenario, rc, remote. */
	void reportMode(const char* mode);

	// ---- outgoing, safe from any thread ----
	void sendJson(const std::string& json);
	void ack(uint32_t id, const std::string& dataJson = "null");
	void error(uint32_t id, const char* msg);
	void mirrorBB(BB::State state, BB::Action action, const uint8_t* data, size_t size);

	/** Queue a JPEG frame. Live frames replace an unsent one; snapshots are never dropped. */
	bool sendVideo(const uint8_t* jpeg, size_t len, uint16_t width, uint16_t height, uint32_t seq, bool snapshot);
	/** True while a live frame is still waiting to be sent (lets the camera skip a capture). */
	bool videoPending() const { return pendingVideo.load() != nullptr; }
	bool sendMic(const int16_t* samples, size_t count, uint32_t seq);

	using SpeakerSink = std::function<void(const uint8_t* pcm, size_t bytes)>;
	void setSpeakerSink(SpeakerSink sink);

	static std::string escape(const std::string& s);

protected:
	void tick(float deltaTime) noexcept override;

private:
	inline static LinkService* inst = nullptr;

	enum class Phase : uint8_t { Off, WaitIP, Connect, Online, Backoff };
	Phase phase = Phase::Off;

	LinkConfigData config{};
	std::mutex configMutex;
	std::atomic_bool configChanged = false;
	std::atomic_bool hostChanged = false;
	std::atomic_bool radioUp = false;
	std::atomic<uint64_t> reconnectAt = 0;
	uint64_t nextRssiReport = 0;
	std::mutex radioMutex;
	// Wi-Fi start-up needs internal DMA RAM; refuse below these (bytes). Tunable over BLE.
	// Wi-Fi start-up failed with 27.4 KB free (block 17 KB); start conservatively, tune down if needed.
	std::atomic_uint32_t minFree = 34 * 1024;
	std::atomic_uint32_t minBlock = 16 * 1024;
	std::atomic_bool tuneChanged = false;
	// Channel the network was last joined on, so the next join can skip the full scan.
	std::atomic<uint8_t> lastChannel = 0;
	std::atomic_bool channelChanged = false;
	std::atomic_bool wanted = false;
	std::atomic<uint64_t> wakeUntil = 0;
	std::atomic_uint32_t downCount = 0;

	StrongObjectPtr<class WiFi> wifi;
	std::atomic_bool gotIP = false;
	uint64_t phaseDeadline = 0;
	uint32_t backoffMs = 1000;

	// UDP socket shared with the TX thread; sockMutex guards use against close.
	int sock = -1;
	sockaddr_in target{};
	std::mutex sockMutex;
	std::atomic_bool sockBroken = false;
	std::atomic_bool heardFromPlane = false;
	uint64_t nextHello = 0;
	uint32_t micSeq = 0;
	std::atomic_bool online = false;
	std::atomic_bool remote = false;
	std::atomic_bool resumeAfterRC = false;
	std::atomic_bool aiOnIdle = false;
	const char* lastStartFailure = "";
	std::atomic<uint64_t> hushedAt = 0;
	uint64_t lastRx = 0;

	char mode[12] = "idle";

	// One received datagram
	uint8_t* rxBuf = nullptr;
	static constexpr size_t RxBufSize = 2048;

	// TX
	struct Buf {
		uint8_t* data;
		size_t len;
		bool video;    // whole JPEG frame (seq, ts, w, h, data): split into datagrams when sent
		uint8_t flags;
	};
	/** `interleaveCtrl`: send queued control messages between fragments (not for snapshots sent from there). */
	void sendVideoDatagrams(const Buf* buf, bool interleaveCtrl = true);

	// Replies by request id, so retransmitted requests run once and get their reply again.
	enum class ReplyState : uint8_t { Unknown, Pending, Done };
	struct Reply {
		uint32_t id = 0;
		bool done = false;
		std::string json;
	};
	std::array<Reply, 32> replies{};
	size_t replyNext = 0;
	std::mutex replyMutex;
	ReplyState lookupReply(uint32_t id, std::string& out);
	void rememberRequest(uint32_t id);
	void rememberReply(uint32_t id, const std::string& json);
	void clearReplies();
	QueueHandle_t ctrlQueue = nullptr;
	QueueHandle_t micQueue = nullptr;
	std::atomic<Buf*> pendingVideo = nullptr;
	SemaphoreHandle_t txWake = nullptr;
	std::unique_ptr<Threaded> txThread;
	void txLoop();
	/** Send queued control messages; video sending calls this between fragments. */
	void sendPendingCtrl();
	/** Send one datagram, waiting up to `waitMs` for a free Wi-Fi TX buffer. */
	bool sendRaw(const uint8_t* data, size_t len, uint32_t waitMs = 20);
	static Buf* makeBuf(size_t len);
	static void freeBuf(Buf* buf);
	void enqueue(QueueHandle_t queue, Buf* buf);
	void drainQueues();

	std::mutex sinkMutex;
	SpeakerSink speakerSink;

	uint32_t micSeqFallback = 0;

	bool openSocket();
	void sendHelloDatagram();
	void closeSocket();
	void serviceSocket();
	void handleFrame(LinkProto::FrameType type, uint8_t flags, const uint8_t* payload, size_t len);
	void handleCtrl(const char* json, size_t len);
	void sendHello();
	void sendStatusSnapshot();



	// Telemetry from services, bound in postInitProperties
	void onBatteryLevel(Battery::Level level);
	void onChargeStatus(ChargingState state);
	void onProximityChange(ProxSensor sensor, bool covered);
	void onProximityReading(ProxState front, ProxState bottom);
	void onMotion(MotionType type);
	void onButton(int button, ISRButtonInput::Action action);
	void sendBattery();

	ProxState proxFront = ProxState::Unknown;
	ProxState proxBottom = ProxState::Unknown;
};

#endif //BUTTERBOT_FIRMWARE_LINKSERVICE_H

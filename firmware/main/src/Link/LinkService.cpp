#include "LinkService.h"
#include <cstring>
#include <cstdio>
#include <cJSON.h>
#include <mbedtls/base64.h>
#include <esp_heap_caps.h>
#include <esp_mac.h>
#include <esp_wifi.h>
#include <lwip/sockets.h>
#include <lwip/netdb.h>
#include <Periphery/WiFi.h>
#include <Core/Application.h>
#include <Log/Log.h>
#include <Util/stdafx.h>
#include "LinkCredentials.h"

DEFINE_LOG(LinkService)

namespace {
	constexpr uint32_t MaxBackoffMs = 15000;
	// The plane pings every 2 s; silence for longer than this means the link is dead.
	constexpr uint32_t RxTimeoutMs = 8000;
	constexpr int CtrlQueueLen = 32;
	constexpr int MicQueueLen = 16;
	constexpr uint32_t HelloIntervalMs = 1000;
	constexpr uint32_t RssiReportMs = 3000;
	// A wake that has not reached the plane by then is abandoned (Wi-Fi off again).
	constexpr uint32_t WakeTimeoutMs = 60000;
	// After losing an established link, keep retrying this long before giving up.
	constexpr uint32_t ReconnectWindowMs = 20000;
	// Measured need of esp_wifi_init() + station start with this sdkconfig, plus margin.

	const char* proxName(ProxState s){
		switch(s){
			case ProxState::Covered: return "covered";
			case ProxState::Uncovered: return "uncovered";
			default: return "unknown";
		}
	}

	const char* chargeName(ChargingState s){
		switch(s){
			case ChargingState::Charging: return "charging";
			case ChargingState::Full: return "full";
			default: return "unplugged";
		}
	}

	framesize_t parseRes(const char* s){
		if(s == nullptr) return FRAMESIZE_QVGA;
		if(!strcmp(s, "r128")) return FRAMESIZE_128X128;
		if(!strcmp(s, "qvga")) return FRAMESIZE_QVGA;
		if(!strcmp(s, "vga")) return FRAMESIZE_VGA;
		if(!strcmp(s, "svga")) return FRAMESIZE_SVGA;
		if(!strcmp(s, "xga")) return FRAMESIZE_XGA;
		if(!strcmp(s, "hd")) return FRAMESIZE_HD;
		if(!strcmp(s, "uxga")) return FRAMESIZE_UXGA;
		if(!strcmp(s, "qxga")) return FRAMESIZE_QXGA;
		return FRAMESIZE_QVGA;
	}

	struct TypeName {
		const char* name;
		LinkRequest::Type type;
	};

	constexpr TypeName RequestTypes[] = {
		{ "enter_remote", LinkRequest::Type::EnterRemote },
		{ "exit_remote", LinkRequest::Type::ExitRemote },
		{ "drive", LinkRequest::Type::Drive },
		{ "stop", LinkRequest::Type::Stop },
		{ "say", LinkRequest::Type::Say },
		{ "spk_begin", LinkRequest::Type::SpkBegin },
		{ "spk_end", LinkRequest::Type::SpkEnd },
		{ "spk_abort", LinkRequest::Type::SpkAbort },
		{ "volume", LinkRequest::Type::Volume },
		{ "camera", LinkRequest::Type::Camera },
		{ "snapshot", LinkRequest::Type::Snapshot },
		{ "mic", LinkRequest::Type::Mic },
		{ "led", LinkRequest::Type::Led },
		{ "status_light", LinkRequest::Type::StatusLight },
		{ "phone", LinkRequest::Type::Phone },
		{ "ai_status", LinkRequest::Type::AiStatus },
		{ "home_info", LinkRequest::Type::HomeInfo },
		{ "peek", LinkRequest::Type::Peek },
		{ "scenario", LinkRequest::Type::Scenario },
		{ "ir_list", LinkRequest::Type::IrList },
		{ "ir_send", LinkRequest::Type::IrSend },
		{ "modules", LinkRequest::Type::Modules },
		{ "safety", LinkRequest::Type::Safety },
	};

	double num(const cJSON* obj, const char* key, double def){
		const cJSON* v = cJSON_GetObjectItemCaseSensitive(obj, key);
		return cJSON_IsNumber(v) ? v->valuedouble : def;
	}

	bool flag(const cJSON* obj, const char* key, bool def){
		const cJSON* v = cJSON_GetObjectItemCaseSensitive(obj, key);
		return cJSON_IsBool(v) ? cJSON_IsTrue(v) : def;
	}

	const char* str(const cJSON* obj, const char* key){
		const cJSON* v = cJSON_GetObjectItemCaseSensitive(obj, key);
		return cJSON_IsString(v) ? v->valuestring : nullptr;
	}
}

// Stack in PSRAM: this thread never touches flash. Wi-Fi start/stop and NVS writes (which need an
// internal-RAM stack) run on the state machine thread, like the stock RC mode does.
LinkService::LinkService() : AsyncEntity(1, 6 * 1024, 6, 0, /*internalStack=*/false){
	inst = this;

	// Keep JSON parsing out of internal RAM (an allocation failure there aborts the firmware).
	static cJSON_Hooks hooks = {
		.malloc_fn = [](size_t size){ return heap_caps_malloc(size, MALLOC_CAP_SPIRAM | MALLOC_CAP_8BIT); },
		.free_fn = free,
	};
	cJSON_InitHooks(&hooks);

	txWake = xSemaphoreCreateCounting(64, 0);
	ctrlQueue = xQueueCreate(CtrlQueueLen, sizeof(Buf*));
	micQueue = xQueueCreate(MicQueueLen, sizeof(Buf*));
	rxBuf = static_cast<uint8_t*>(heap_caps_malloc(RxBufSize, MALLOC_CAP_SPIRAM));

	setEventScanningTime(5);

	txThread = std::make_unique<Threaded>([this](){ txLoop(); }, "LinkTx", 0, 6 * 1024, 6, 1, false);
	txThread->start();

	// Wi-Fi only runs on request (wake / provisioning), like the stock RC mode: there is not
	// enough internal RAM to keep it up next to the idle behaviours.
	if(LinkCredentials::wasWiFiStarting()){
		LinkCredentials::setWiFiStarting(false);
		CMF_LOG(LinkService, LogLevel::Error, "the previous Wi-Fi start crashed the firmware");
	}

	lastChannel = LinkCredentials::loadChannel();

	LinkTuneData tuneData{};
	if(LinkCredentials::loadTune(tuneData)){
		if(tuneData.minFree) minFree = std::max<uint32_t>(tuneData.minFree, 16 * 1024);
		if(tuneData.minBlock) minBlock = std::max<uint32_t>(tuneData.minBlock, 8 * 1024);
	}
	CMF_LOG(LinkService, LogLevel::Info, "Wi-Fi needs internal free >= %lu, block >= %lu", (unsigned long) minFree, (unsigned long) minBlock);

	LinkConfigData stored{};
	if(LinkCredentials::load(stored)){
		std::lock_guard lock(configMutex);
		config = stored;
		CMF_LOG(LinkService, LogLevel::Info, "link configured for \"%s\" -> %s:%u", config.SSID, config.host, config.port);
	}else{
		CMF_LOG(LinkService, LogLevel::Info, "no link configuration stored; provision with bbplane");
	}
}

LinkService::~LinkService(){
	txThread->stop(portMAX_DELAY);
	closeSocket();
	stopRadio();
	drainQueues();
	vQueueDelete(ctrlQueue);
	vQueueDelete(micQueue);
	vSemaphoreDelete(txWake);
	free(rxBuf);
	inst = nullptr;
}

void LinkService::postInitProperties() noexcept{
	Super::postInitProperties();

	const Application* app = Application::getApp();
	if(auto* battery = app->getService<Battery>()){
		battery->OnLevelChanged.bind(this, &LinkService::onBatteryLevel);
		battery->OnChargeStatus.bind(this, &LinkService::onChargeStatus);
	}
	if(auto* base = app->getService<BaseBoard>()){
		base->onProximityChange.bind(this, &LinkService::onProximityChange);
		base->onProximityReading.bind(this, &LinkService::onProximityReading);
	}
	if(auto* motion = app->getService<MotionService>()){
		motion->onMotion.bind(this, &LinkService::onMotion);
	}
	if(auto* button = app->getService<ISRButtonInput>()){
		button->OnButtonEvent.bind(this, &LinkService::onButton);
	}
}

// ---------------------------------------------------------------- configuration

void LinkService::tune(const LinkTuneData& data){
	// Floors: below these esp_wifi_init() can run out of memory, which aborts the firmware.
	if(data.minFree) minFree = std::max<uint32_t>(data.minFree, 16 * 1024);
	if(data.minBlock) minBlock = std::max<uint32_t>(data.minBlock, 8 * 1024);
	tuneChanged = true;
}

void LinkService::configure(const LinkConfigData& cfg){
	{
		std::lock_guard lock(configMutex);
		config = cfg;
	}
	configChanged = true;
}

uint8_t LinkService::requestWake(){
	{
		std::lock_guard lock(configMutex);
		if(!LinkCredentials::isValid(config)){
			return 0;
		}
	}
	if(remote){
		return 1; // already in AI mode
	}
	if(strcmp(mode, "idle") != 0){
		return 2;
	}
	OnWakeRequest.broadcast();
	return 1;
}

bool LinkService::wake(){
	// Wi-Fi only runs inside RemoteState: autonomy, RC and voice commands are off there.
	if(!remote){
		return false;
	}
	{
		std::lock_guard lock(configMutex);
		if(!LinkCredentials::isValid(config)){
			return false;
		}
	}
	wakeUntil = millis() + WakeTimeoutMs;
	wanted = true;
	return true;
}

void LinkService::sleep(){
	wanted = false;
}

bool LinkService::suspend(){
	wanted = false;
	// tick() bumps downCount every pass it spends with Wi-Fi torn down.
	const uint32_t start = downCount;
	for(int i = 0; i < 80; i++){
		if(downCount != start){
			return true;
		}
		vTaskDelay(pdMS_TO_TICKS(100));
	}
	CMF_LOG(LinkService, LogLevel::Error, "suspend timed out");
	return false;
}

void LinkService::reportMode(const char* m){
	strncpy(mode, m, sizeof(mode) - 1);
	char json[48];
	snprintf(json, sizeof(json), "{\"t\":\"mode\",\"mode\":\"%s\"}", mode);
	sendJson(json);
}

// ---------------------------------------------------------------- connection state machine

void LinkService::tick(float deltaTime) noexcept{
	if(hostChanged){
		// New plane address: reconnect with it (the radio is re-targeted by persist()).
		hostChanged = false;
		closeSocket();
		if(phase == Phase::Online || phase == Phase::Connect || phase == Phase::Backoff){
			phase = Phase::Backoff;
			phaseDeadline = millis() + 500;
		}
	}

	const uint64_t now = millis();

	if(reconnectAt != 0 && now >= reconnectAt){
		reconnectAt = 0;
		// esp_wifi_connect() posts to the Wi-Fi task and does not touch flash.
		std::lock_guard lock(radioMutex);
		if(wifi && radioUp){
			wifi->connect();
		}
	}

	if(!wanted){
		if(online){
			// Let queued control messages (e.g. the exit_remote ack) go out before closing.
			for(int i = 0; i < 20 && online && !sockBroken && uxQueueMessagesWaiting(ctrlQueue) > 0; i++){
				vTaskDelay(pdMS_TO_TICKS(10));
			}
			closeSocket();
			CMF_LOG(LinkService, LogLevel::Info, "link off");
		}
		phase = Phase::Off;
		downCount++;
		vTaskDelay(pdMS_TO_TICKS(100));
		return;
	}

	if(phase != Phase::Online && wakeUntil != 0 && now > wakeUntil){
		CMF_LOG(LinkService, LogLevel::Warning, "could not reach the plane, giving up until the next wake");
		wanted = false;
		wakeUntil = 0;
		return;
	}

	switch(phase){
		case Phase::Off:
			backoffMs = 1000;
			if(radioUp){
				phase = Phase::WaitIP;
			}else{
				vTaskDelay(pdMS_TO_TICKS(50)); // RemoteState starts the radio
			}
			break;

		case Phase::WaitIP:
			if(!radioUp){
				phase = Phase::Off;
			}else if(gotIP){
				phase = Phase::Connect;
			}else{
				vTaskDelay(pdMS_TO_TICKS(20)); // the Wi-Fi driver keeps retrying (onStaDisconnected)
			}
			break;

		case Phase::Connect:
			if(!gotIP){
				phase = Phase::WaitIP;
				break;
			}
			if(sock < 0 && !openSocket()){
				phase = Phase::Backoff;
				phaseDeadline = now + backoffMs;
				backoffMs = std::min<uint32_t>(backoffMs * 2, MaxBackoffMs);
				break;
			}
			// Announce ourselves until the plane answers (it pings every second once it knows us).
			if(now >= nextHello){
				nextHello = now + HelloIntervalMs;
				sendHelloDatagram();
			}
			serviceSocket();
			if(heardFromPlane){
				backoffMs = 1000;
				wakeUntil = 0;
				lastRx = millis();
				online = true;
				phase = Phase::Online;
				CMF_LOG(LinkService, LogLevel::Info, "plane answered");
				sendHello();
				sendStatusSnapshot();
				OnConnection.broadcast(true);
			}
			break;

		case Phase::Online:
			serviceSocket();
			if(now >= nextRssiReport){
				nextRssiReport = now + RssiReportMs;
				int rssi = 0;
				bool have = false;
				{
					std::lock_guard lock(radioMutex);
					if(wifi){
						rssi = wifi->getConnectionRSSI();
						have = true;
					}
				}
				if(have){
					// Internal RAM is what runs out first (Wi-Fi, Bluetooth and DMA need it).
					char json[112];
					snprintf(json, sizeof(json), "{\"t\":\"wifi\",\"rssi\":%d,\"ram_free\":%u,\"ram_block\":%u}", rssi,
							 (unsigned) heap_caps_get_free_size(MALLOC_CAP_INTERNAL | MALLOC_CAP_DMA),
							 (unsigned) heap_caps_get_largest_free_block(MALLOC_CAP_INTERNAL | MALLOC_CAP_DMA));
					sendJson(json);
				}
			}
			if(!gotIP || millis() - lastRx > RxTimeoutMs){
				// UDP has no connection to break: silence from the plane is the only signal.
				CMF_LOG(LinkService, LogLevel::Warning, "plane silent (%s)", !gotIP ? "Wi-Fi lost" : "timeout");
				online = false;
				heardFromPlane = false;
				// Before anything new is received: retransmitted requests then register afresh.
				clearReplies();
				drainQueues();
				OnConnection.broadcast(false);
				phase = gotIP ? Phase::Connect : Phase::WaitIP;
				wakeUntil = millis() + ReconnectWindowMs;
			}
			break;

		case Phase::Backoff:
			if(now >= phaseDeadline){
				phase = radioUp ? (gotIP ? Phase::Connect : Phase::WaitIP) : Phase::Off;
			}else{
				vTaskDelay(pdMS_TO_TICKS(50));
			}
			break;
	}
}

// ---------------------------------------------------------------- radio (state machine thread)

void LinkService::persist(){
	if(tuneChanged){
		tuneChanged = false;
		const LinkTuneData data{ .minFree = minFree, .minBlock = minBlock };
		LinkCredentials::storeTune(data);
		CMF_LOG(LinkService, LogLevel::Info, "Wi-Fi RAM thresholds now free >= %lu, block >= %lu", (unsigned long) data.minFree, (unsigned long) data.minBlock);
	}

	if(channelChanged){
		channelChanged = false;
		LinkCredentials::storeChannel(lastChannel);
	}

	if(configChanged){
		configChanged = false;
		// Possibly another network: find it with a full scan.
		lastChannel = 0;
		LinkCredentials::storeChannel(0);
		LinkConfigData cfg;
		{
			std::lock_guard lock(configMutex);
			cfg = config;
		}
		const bool ok = LinkCredentials::store(cfg);
		CMF_LOG(LinkService, LogLevel::Info, "new link configuration (%s): \"%s\" -> %s:%u", ok ? "stored" : "NOT stored", cfg.SSID, cfg.host, cfg.port);

		std::lock_guard lock(radioMutex);
		if(wifi){
			// Join the new network (settings are kept in RAM only, no flash write).
			gotIP = false;
			wifi->disconnect();
			wifi->setTargetParameters(cfg.SSID, cfg.password);
			wifi->connect();
		}
		hostChanged = true;
	}
}

bool LinkService::startRadio(){
	std::lock_guard lock(radioMutex);
	if(wifi){
		return true;
	}

	LinkConfigData cfg;
	{
		std::lock_guard cfgLock(configMutex);
		cfg = config;
	}
	if(!LinkCredentials::isValid(cfg)){
		lastStartFailure = "settings";
		return false;
	}

	// Internal DMA RAM is the scarce resource on this board. esp_wifi_init() aborts the whole
	// firmware when it runs out (and the robot then powers itself off), so refuse instead.
	const size_t freeInternal = heap_caps_get_free_size(MALLOC_CAP_INTERNAL | MALLOC_CAP_DMA);
	const size_t largest = heap_caps_get_largest_free_block(MALLOC_CAP_INTERNAL | MALLOC_CAP_DMA);
	CMF_LOG(LinkService, LogLevel::Warning, "starting Wi-Fi: internal free %u, largest block %u", (unsigned) freeInternal, (unsigned) largest);
	if(freeInternal < minFree || largest < minBlock){
		CMF_LOG(LinkService, LogLevel::Error, "not enough internal RAM for Wi-Fi (need free %lu, block %lu), not starting it",
				(unsigned long) minFree, (unsigned long) minBlock);
		lastStartFailure = "memory";
		return false;
	}

	gotIP = false;
	LinkCredentials::setWiFiStarting(true); // cleared by stopRadio() / radioConfirmed()
	// Its events (got IP, disconnected) are bound by RemoteState on the state machine thread, the
	// same thread that deletes it in stopRadio(), so a handler can never run on a freed object.
	wifi = newObject<WiFi>(this);
	wifi->setIPMode(WiFiIPMode::DHCP);
	if(!wifi->startStation()){
		CMF_LOG(LinkService, LogLevel::Error, "Wi-Fi start failed");
		lastStartFailure = "wifi";
		delete *wifi;
		wifi = nullptr;
		LinkCredentials::setWiFiStarting(false);
		return false;
	}
	CMF_LOG(LinkService, LogLevel::Warning, "Wi-Fi started: internal free %u, largest block %u",
			(unsigned) heap_caps_get_free_size(MALLOC_CAP_INTERNAL | MALLOC_CAP_DMA),
			(unsigned) heap_caps_get_largest_free_block(MALLOC_CAP_INTERNAL | MALLOC_CAP_DMA));
	wifi->setTargetParameters(cfg.SSID, cfg.password, lastChannel);
	wifi->connect();
	radioUp = true;
	CMF_LOG(LinkService, LogLevel::Info, "joining Wi-Fi \"%s\"", cfg.SSID);
	return true;
}

void LinkService::stopRadio(){
	std::lock_guard lock(radioMutex);
	radioUp = false;
	gotIP = false;
	reconnectAt = 0;
	if(!wifi){
		return;
	}
	delete *wifi;
	wifi = nullptr;
	LinkCredentials::setWiFiStarting(false);
	CMF_LOG(LinkService, LogLevel::Info, "Wi-Fi off");
}

void LinkService::radioConfirmed(){
	LinkCredentials::setWiFiStarting(false);
}

WiFi* LinkService::radio(){
	return *wifi;
}

void LinkService::onGotIP(uint32_t ip){
	gotIP = ip != 0;
	if(gotIP){
		CMF_LOG(LinkService, LogLevel::Info, "got IP " IPSTR, IP2STR(reinterpret_cast<esp_ip4_addr_t*>(&ip)));
		// Remember the channel for a fast join next time, but only with a good signal: a fast
		// scan takes the first match on that channel, so a weak one would stick forever. Without
		// it the next join scans everything and picks the strongest access point.
		wifi_ap_record_t ap{};
		if(esp_wifi_sta_get_ap_info(&ap) == ESP_OK){
			const uint8_t channel = ap.rssi >= -70 ? ap.primary : 0;
			if(channel != lastChannel){
				lastChannel = channel;
				channelChanged = true; // stored by persist() on the state machine thread
			}
		}
	}
}

void LinkService::onStaDisconnected(uint8_t reason){
	gotIP = false;
	CMF_LOG(LinkService, LogLevel::Info, "Wi-Fi disconnected (reason %u), reconnecting", reason);
	// The driver does not retry on its own; the link thread reconnects shortly (never block here).
	reconnectAt = millis() + 500;
}

bool LinkService::openSocket(){
	LinkConfigData cfg;
	{
		std::lock_guard lock(configMutex);
		cfg = config;
	}

	if(rxBuf == nullptr){
		return false;
	}

	sockaddr_in addr{};
	addr.sin_family = AF_INET;
	addr.sin_port = htons(cfg.port);
	if(inet_aton(cfg.host, &addr.sin_addr) == 0){
		// Only names need DNS (and may take a while); IP addresses are used directly.
		char port[8];
		snprintf(port, sizeof(port), "%u", cfg.port);
		addrinfo hints{};
		hints.ai_family = AF_INET;
		hints.ai_socktype = SOCK_DGRAM;
		addrinfo* res = nullptr;
		if(getaddrinfo(cfg.host, port, &hints, &res) != 0 || res == nullptr){
			CMF_LOG(LinkService, LogLevel::Warning, "cannot resolve plane host %s", cfg.host);
			return false;
		}
		memcpy(&addr, res->ai_addr, std::min<size_t>(sizeof(addr), res->ai_addrlen));
		freeaddrinfo(res);
	}

	const int s = ::socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP);
	if(s < 0){
		CMF_LOG(LinkService, LogLevel::Error, "socket() failed: %d", errno);
		return false;
	}
	sockaddr_in local{};
	local.sin_family = AF_INET;
	local.sin_addr.s_addr = htonl(INADDR_ANY);
	local.sin_port = 0;
	::bind(s, reinterpret_cast<sockaddr*>(&local), sizeof(local));
	fcntl(s, F_SETFL, fcntl(s, F_GETFL, 0) | O_NONBLOCK);

	{
		std::lock_guard lock(sockMutex);
		sock = s;
		target = addr;
	}
	heardFromPlane = false;
	nextHello = 0;
	clearReplies();
	CMF_LOG(LinkService, LogLevel::Info, "UDP link to plane %s:%u", cfg.host, cfg.port);
	return true;
}

void LinkService::closeSocket(){
	const bool wasOnline = online;
	online = false;
	heardFromPlane = false;
	{
		std::lock_guard lock(sockMutex);
		if(sock >= 0){
			::close(sock);
			sock = -1;
		}
	}
	drainQueues();

	if(wasOnline){
		OnConnection.broadcast(false);
	}
}

void LinkService::serviceSocket(){
	if(sock < 0){
		vTaskDelay(pdMS_TO_TICKS(20));
		return;
	}
	fd_set rfds;
	FD_ZERO(&rfds);
	FD_SET(sock, &rfds);
	timeval tv{ .tv_sec = 0, .tv_usec = 20 * 1000 };
	if(select(sock + 1, &rfds, nullptr, nullptr, &tv) <= 0){
		return;
	}

	// One frame per datagram; drain what is queued.
	for(int i = 0; i < 16; i++){
		sockaddr_in from{};
		socklen_t fromLen = sizeof(from);
		const int n = ::recvfrom(sock, rxBuf, RxBufSize, 0, reinterpret_cast<sockaddr*>(&from), &fromLen);
		if(n <= 0){
			break;
		}
		if(from.sin_addr.s_addr != target.sin_addr.s_addr || (size_t) n < LinkProto::HeaderLen){
			continue; // not from our plane
		}
		if(LinkProto::getU16(rxBuf) != LinkProto::Magic){
			continue;
		}
		const uint32_t len = LinkProto::getU32(rxBuf + 4);
		if(len > (size_t) n - LinkProto::HeaderLen){
			continue;
		}
		lastRx = millis();
		heardFromPlane = true;
		handleFrame(static_cast<LinkProto::FrameType>(rxBuf[2]), rxBuf[3], rxBuf + LinkProto::HeaderLen, len);
	}
}

void LinkService::handleFrame(LinkProto::FrameType type, uint8_t flags, const uint8_t* payload, size_t len){
	switch(type){
		case LinkProto::FrameType::Ctrl:
			handleCtrl(reinterpret_cast<const char*>(payload), len);
			break;
		case LinkProto::FrameType::SpkPcm: {
			std::lock_guard lock(sinkMutex);
			if(speakerSink){
				speakerSink(payload, len);
			}
			break;
		}
		default:
			break;
	}
}

void LinkService::handleCtrl(const char* json, size_t len){
	cJSON* root = cJSON_ParseWithLength(json, len);
	if(root == nullptr){
		CMF_LOG(LinkService, LogLevel::Warning, "unparseable ctrl message");
		return;
	}

	const char* t = str(root, "t");
	const uint32_t id = (uint32_t) num(root, "id", 0);

	if(t != nullptr && !strcmp(t, "hello")){
		// The plane (re)started while we were connected: introduce ourselves again.
		sendHello();
		sendStatusSnapshot();
		cJSON_Delete(root);
		return;
	}

	if(t != nullptr && !strcmp(t, "ping")){
		char out[80];
		snprintf(out, sizeof(out), "{\"t\":\"pong\",\"id\":%lu,\"ts\":%.0f}", id, num(root, "ts", 0));
		sendJson(out);
		cJSON_Delete(root);
		return;
	}

	// The plane retransmits requests until they are acked: run each id once, re-send its reply.
	if(id != 0){
		std::string cached;
		const ReplyState state = lookupReply(id, cached);
		if(state == ReplyState::Done){
			sendJson(cached);
			cJSON_Delete(root);
			return;
		}
		if(state == ReplyState::Pending){
			cJSON_Delete(root);
			return;
		}
		rememberRequest(id);
	}

	const TypeName* match = nullptr;
	for(const auto& tn : RequestTypes){
		if(t != nullptr && !strcmp(t, tn.name)){
			match = &tn;
			break;
		}
	}
	if(match == nullptr){
		if(id != 0){
			error(id, "unknown request");
		}
		cJSON_Delete(root);
		return;
	}

	LinkRequest req;
	req.type = match->type;
	req.id = id;

	using T = LinkRequest::Type;
	if(req.type == T::EnterRemote && remote){
		ack(id);
		reportMode("remote");
		cJSON_Delete(root);
		return;
	}
	if(req.type == T::EnterRemote && strcmp(mode, "idle") != 0){
		char msg[48];
		snprintf(msg, sizeof(msg), "robot is busy (%s)", mode);
		error(id, msg);
		cJSON_Delete(root);
		return;
	}
	if(req.type == T::ExitRemote && !remote){
		ack(id);
		cJSON_Delete(root);
		return;
	}
	if(req.type != T::EnterRemote && req.type != T::ExitRemote && !remote){
		if(id != 0){
			error(id, "not in remote mode");
		}
		cJSON_Delete(root);
		return;
	}

	req.left = (int8_t) std::clamp<double>(num(root, "l", 0), -100, 100);
	req.right = (int8_t) std::clamp<double>(num(root, "r", 0), -100, 100);
	req.ms = (uint16_t) std::clamp<double>(num(root, "ms", 500), 0, 5000);
	req.on = flag(root, "on", false);
	req.gate = flag(root, "gate", true);
	req.gain = (float) std::clamp<double>(num(root, "gain", 1.0), 0.0, 4.0);
	req.level = (float) std::clamp<double>(num(root, "level", 1.0), 0.0, 1.0);
	req.res = parseRes(str(root, "res"));
	req.fps = (uint8_t) std::clamp<double>(num(root, "fps", 10), 1, 30);
	req.quality = (uint8_t) std::clamp<double>(num(root, "quality", 12), 4, 63);
	req.scenario = (uint32_t) num(root, "scenario", 0);
	req.arg = (uint8_t) num(root, "arg", 0);
	req.index = (uint8_t) num(root, "index", 0);
	req.safe = flag(root, "safe", true);
	req.checkTilt = flag(root, "tilt", true);
	req.checkObstacle = flag(root, "obstacle", true);
	if(const char* enc = str(root, "encoder")){
		req.swJpeg = strcmp(enc, "sensor") != 0;
	}
	req.xclkMhz = (uint8_t) std::clamp<double>(num(root, "xclk_mhz", 0), 0, 40);
	if(const cJSON* dma = cJSON_GetObjectItemCaseSensitive(root, "psram_dma"); cJSON_IsBool(dma)){
		req.psramDma = cJSON_IsTrue(dma) ? 1 : 0;
	}
	if(const char* target = str(root, "target")){
		req.ledTarget = !strcmp(target, "module") ? 1 : 0;
	}
	if(const char* m = str(root, "mode")){
		req.ledMode = !strcmp(m, "off") ? 1 : !strcmp(m, "blink") ? 2 : !strcmp(m, "breathe") ? 3 : 0;
	}
	if(req.type == T::StatusLight){
		req.rhythm = LEDRhythm{
			(uint16_t) std::clamp<double>(num(root, "on_ms", 0), 0, 5000),
			(uint16_t) std::clamp<double>(num(root, "off_ms", 0), 0, 5000),
			(uint8_t) std::clamp<double>(num(root, "count", 0), 0, 5),
			(uint16_t) std::clamp<double>(num(root, "period_ms", 1000), 0, 60000),
			(float) std::clamp<double>(num(root, "base", 0), 0, 1),
			(float) std::clamp<double>(num(root, "flash", 1), 0, 1),
		}.sane();
	}
	if(req.type == T::AiStatus){
		req.ai.mode = static_cast<AIStatusData::Mode>(std::clamp<double>(num(root, "mode", 0), 0, 5));
		req.ai.activity = static_cast<AIStatusData::Activity>(std::clamp<double>(num(root, "activity", 2), 0, 3));
		req.ai.focusMinutes = static_cast<uint8_t>(std::clamp<double>(num(root, "focus_minutes", 0), 0, 255));
		req.ai.emotion = static_cast<uint8_t>(std::clamp<double>(num(root, "emotion", 0), 0, 11));
		req.ai.intensity = static_cast<uint8_t>(std::clamp<double>(num(root, "intensity", 0), 0, 100));
	}
	if(req.type == T::HomeInfo){
		// clock: [year, month 1-12, day, hour, minute, second], local time
		const cJSON* clock = cJSON_GetObjectItemCaseSensitive(root, "clock");
		if(cJSON_IsArray(clock) && cJSON_GetArraySize(clock) >= 6){
			const auto at = [clock](int i, int lo, int hi){
				const cJSON* v = cJSON_GetArrayItem(clock, i);
				return std::clamp(cJSON_IsNumber(v) ? v->valueint : lo, lo, hi);
			};
			req.home.clockSet = true;
			req.home.year = at(0, 2000, 2200);
			req.home.month = at(1, 1, 12);
			req.home.day = at(2, 1, 31);
			req.home.hour = at(3, 0, 23);
			req.home.minute = at(4, 0, 59);
			req.home.second = at(5, 0, 59);
		}
		if(const char* v = str(root, "next")) req.home.next = v;
		if(const char* v = str(root, "seen")) req.home.seen = v;
		if(const char* v = str(root, "said")) req.home.said = v;
		req.home.seenSecs = static_cast<uint16_t>(std::clamp<double>(num(root, "seen_secs", 0), 0, UINT16_MAX));
	}
	if(req.type == T::Peek){
		// A small JPEG for the controller's screen, base64 in the JSON.
		if(const char* b64 = str(root, "jpeg")){
			const size_t len = strlen(b64);
			req.text.resize(len * 3 / 4 + 3);
			size_t out = 0;
			if(mbedtls_base64_decode((unsigned char*) req.text.data(), req.text.size(), &out, (const unsigned char*) b64, len) == 0){
				req.text.resize(out);
			}else{
				req.text.clear();
			}
		}
		if(const char* caption = str(root, "caption")){
			req.caption = caption;
		}
	}else if(const char* text = str(root, req.type == T::Phone ? "action" : "text")){
		req.text = text;
	}
	cJSON_Delete(root);

	OnRequest.broadcast(req);
}

void LinkService::sendHelloDatagram(){
	uint8_t mac[6] = {};
	esp_read_mac(mac, ESP_MAC_WIFI_STA);
	char json[256];
	const int len = snprintf(json, sizeof(json),
			 "{\"t\":\"hello\",\"proto\":%lu,\"fw\":\"%s\",\"mac\":\"%02x:%02x:%02x:%02x:%02x:%02x\","
			 "\"caps\":[\"camera\",\"mic\",\"speaker\",\"drive\",\"tts\",\"scenarios\",\"ir\",\"modules\",\"phone\"]}",
			 LinkProto::Version, LinkProto::FirmwareName, mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]);
	uint8_t dgram[LinkProto::HeaderLen + 256];
	LinkProto::putHeader(dgram, LinkProto::FrameType::Ctrl, 0, len);
	memcpy(dgram + LinkProto::HeaderLen, json, len);
	sendRaw(dgram, LinkProto::HeaderLen + len);
}

void LinkService::sendHello(){
	uint8_t mac[6] = {};
	esp_read_mac(mac, ESP_MAC_WIFI_STA);
	char json[256];
	snprintf(json, sizeof(json),
			 "{\"t\":\"hello\",\"proto\":%lu,\"fw\":\"%s\",\"mac\":\"%02x:%02x:%02x:%02x:%02x:%02x\","
			 "\"caps\":[\"camera\",\"mic\",\"speaker\",\"drive\",\"tts\",\"scenarios\",\"ir\",\"modules\",\"phone\"]}",
			 LinkProto::Version, LinkProto::FirmwareName, mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]);
	sendJson(json);
}

void LinkService::sendStatusSnapshot(){
	char json[48];
	snprintf(json, sizeof(json), "{\"t\":\"mode\",\"mode\":\"%s\"}", remote ? "remote" : mode);
	sendJson(json);
	sendBattery();
	// Proximity: RemoteState requests a reading on connect. BaseBoard I2C errors abort, so it is
	// only called from the state machine thread, like the stock code does.
}

// ---------------------------------------------------------------- telemetry

void LinkService::sendBattery(){
	auto* battery = Application::getApp()->getService<Battery>();
	if(battery == nullptr){
		return;
	}
	char json[112];
	snprintf(json, sizeof(json), "{\"t\":\"battery\",\"pct\":%.1f,\"level\":%u,\"charging\":\"%s\"}",
			 battery->getPerc(), (unsigned) battery->getLevel(), chargeName(battery->getChargingState()));
	sendJson(json);
}

void LinkService::onBatteryLevel(Battery::Level level){
	sendBattery();
}

void LinkService::onChargeStatus(ChargingState state){
	sendBattery();
}

void LinkService::onProximityChange(ProxSensor sensor, bool covered){
	if(sensor == ProxSensor::Front){
		proxFront = covered ? ProxState::Covered : ProxState::Uncovered;
	}else{
		proxBottom = covered ? ProxState::Covered : ProxState::Uncovered;
	}
	onProximityReading(proxFront, proxBottom);
}

void LinkService::onProximityReading(ProxState front, ProxState bottom){
	proxFront = front;
	proxBottom = bottom;
	char json[80];
	snprintf(json, sizeof(json), "{\"t\":\"proximity\",\"front\":\"%s\",\"bottom\":\"%s\"}", proxName(front), proxName(bottom));
	sendJson(json);
}

void LinkService::onMotion(MotionType type){
	static constexpr const char* Names[] = { "fall", "upside_down", "pickup", "shake" };
	char json[64];
	snprintf(json, sizeof(json), "{\"t\":\"motion\",\"kind\":\"%s\"}", Names[std::min<uint8_t>((uint8_t) type, 3)]);
	sendJson(json);
}

void LinkService::onButton(int button, ISRButtonInput::Action action){
	char json[64];
	snprintf(json, sizeof(json), "{\"t\":\"button\",\"action\":\"%s\"}", action == ISRButtonInput::Action::Press ? "press" : "release");
	sendJson(json);
}

void LinkService::mirrorBB(BB::State state, BB::Action action, const uint8_t* data, size_t size){
	if(!online){
		return;
	}
	// AI mode's status and the home screen's news come from the plane in the first place.
	if(state == BB::State::AI || (state == BB::State::Idle && action.idle == BB::Action::Idle::Home)){
		return;
	}
	uint32_t actionRaw = 0;
	memcpy(&actionRaw, &action, std::min(sizeof(actionRaw), sizeof(action)));
	std::string json = "{\"t\":\"bb\",\"state\":" + std::to_string((uint32_t) state) + ",\"action\":" + std::to_string(actionRaw) + ",\"data\":\"";
	static constexpr char Hex[] = "0123456789abcdef";
	for(size_t i = 0; i < size; i++){
		json += Hex[data[i] >> 4];
		json += Hex[data[i] & 0xf];
	}
	json += "\"}";
	sendJson(json);
}

// ---------------------------------------------------------------- outgoing

std::string LinkService::escape(const std::string& s){
	std::string out;
	out.reserve(s.size() + 8);
	for(const char c : s){
		switch(c){
			case '"': out += "\\\""; break;
			case '\\': out += "\\\\"; break;
			case '\n': out += "\\n"; break;
			case '\r': out += "\\r"; break;
			case '\t': out += "\\t"; break;
			default:
				if((uint8_t) c < 0x20){
					char buf[8];
					snprintf(buf, sizeof(buf), "\\u%04x", c);
					out += buf;
				}else{
					out += c;
				}
		}
	}
	return out;
}

LinkService::Buf* LinkService::makeBuf(size_t len){
	// A failed allocation aborts the firmware (CONFIG_HEAP_ABORT_WHEN_ALLOCATION_FAILS); drop instead.
	if(heap_caps_get_largest_free_block(MALLOC_CAP_SPIRAM) < len + 1024){
		return nullptr;
	}
	auto* buf = static_cast<Buf*>(heap_caps_malloc(sizeof(Buf), MALLOC_CAP_SPIRAM));
	if(buf == nullptr){
		return nullptr;
	}
	buf->data = static_cast<uint8_t*>(heap_caps_malloc(len, MALLOC_CAP_SPIRAM));
	if(buf->data == nullptr){
		free(buf);
		return nullptr;
	}
	buf->len = len;
	buf->video = false;
	buf->flags = 0;
	return buf;
}

void LinkService::freeBuf(Buf* buf){
	if(buf == nullptr){
		return;
	}
	free(buf->data);
	free(buf);
}

void LinkService::enqueue(QueueHandle_t queue, Buf* buf){
	if(buf == nullptr){
		return;
	}
	if(!online || xQueueSend(queue, &buf, 0) != pdTRUE){
		freeBuf(buf);
		return;
	}
	xSemaphoreGive(txWake);
}

void LinkService::sendJson(const std::string& json){
	if(!online){
		return;
	}
	if(json.size() > LinkProto::MaxDatagramPayload){
		CMF_LOG(LinkService, LogLevel::Warning, "control message too large for one datagram (%u), dropped", (unsigned) json.size());
		return;
	}
	Buf* buf = makeBuf(LinkProto::HeaderLen + json.size());
	if(buf == nullptr){
		return;
	}
	LinkProto::putHeader(buf->data, LinkProto::FrameType::Ctrl, 0, json.size());
	memcpy(buf->data + LinkProto::HeaderLen, json.data(), json.size());
	enqueue(ctrlQueue, buf);
}

void LinkService::ack(uint32_t id, const std::string& dataJson){
	const std::string json = "{\"t\":\"ack\",\"id\":" + std::to_string(id) + ",\"data\":" + dataJson + "}";
	rememberReply(id, json);
	sendJson(json);
}

void LinkService::error(uint32_t id, const char* msg){
	const std::string json = "{\"t\":\"error\",\"id\":" + std::to_string(id) + ",\"msg\":\"" + escape(msg) + "\"}";
	rememberReply(id, json);
	sendJson(json);
}

LinkService::ReplyState LinkService::lookupReply(uint32_t id, std::string& out){
	std::lock_guard lock(replyMutex);
	for(const Reply& r : replies){
		if(r.id == id){
			if(r.done){
				out = r.json;
				return ReplyState::Done;
			}
			return ReplyState::Pending;
		}
	}
	return ReplyState::Unknown;
}

void LinkService::rememberRequest(uint32_t id){
	std::lock_guard lock(replyMutex);
	Reply& r = replies[replyNext];
	replyNext = (replyNext + 1) % replies.size();
	r.id = id;
	r.done = false;
	r.json.clear();
}

void LinkService::rememberReply(uint32_t id, const std::string& json){
	std::lock_guard lock(replyMutex);
	for(Reply& r : replies){
		if(r.id == id){
			r.done = true;
			r.json = json;
			return;
		}
	}
}

void LinkService::clearReplies(){
	std::lock_guard lock(replyMutex);
	for(Reply& r : replies){
		r.id = 0;
		r.done = false;
		r.json.clear();
	}
}

bool LinkService::sendVideo(const uint8_t* jpeg, size_t len, uint16_t width, uint16_t height, uint32_t seq, bool snapshot){
	if(!online){
		return false;
	}
	// Whole frame queued as one item; the TX thread splits it into datagrams.
	Buf* buf = makeBuf(LinkProto::MediaHeaderLen + 4 + len);
	if(buf == nullptr){
		return false;
	}
	buf->video = true;
	buf->flags = snapshot ? LinkProto::FlagSnapshot : 0;
	uint8_t* p = buf->data;
	LinkProto::putU32(p, seq);
	LinkProto::putU32(p + 4, (uint32_t) millis());
	LinkProto::putU16(p + 8, width);
	LinkProto::putU16(p + 10, height);
	memcpy(p + 12, jpeg, len);

	if(snapshot){
		enqueue(ctrlQueue, buf);
		return true;
	}

	// Live view: newest frame wins.
	freeBuf(pendingVideo.exchange(buf));
	xSemaphoreGive(txWake);
	return true;
}

bool LinkService::sendMic(const int16_t* samples, size_t count, uint32_t seq){
	if(!online){
		return false;
	}
	// Small datagrams: a lost one is a 32 ms gap.
	for(size_t off = 0; off < count; off += LinkProto::MicSamplesPerDatagram){
		const size_t n = std::min(count - off, LinkProto::MicSamplesPerDatagram);
		const size_t payload = LinkProto::MediaHeaderLen + n * sizeof(int16_t);
		Buf* buf = makeBuf(LinkProto::HeaderLen + payload);
		if(buf == nullptr){
			return false;
		}
		uint8_t* p = buf->data;
		LinkProto::putHeader(p, LinkProto::FrameType::MicPcm, 0, payload);
		p += LinkProto::HeaderLen;
		LinkProto::putU32(p, ++micSeq);
		LinkProto::putU32(p + 4, (uint32_t) millis());
		memcpy(p + LinkProto::MediaHeaderLen, samples + off, n * sizeof(int16_t)); // ESP32 is little-endian
		enqueue(micQueue, buf);
	}
	return true;
}

void LinkService::setSpeakerSink(SpeakerSink sink){
	std::lock_guard lock(sinkMutex);
	speakerSink = std::move(sink);
}

bool LinkService::sendRaw(const uint8_t* data, size_t len, uint32_t waitMs){
	std::lock_guard lock(sockMutex);
	if(sock < 0){
		return false;
	}
	// Wi-Fi has only CONFIG_ESP_WIFI_STATIC_TX_BUFFER_NUM (2) TX buffers, so sendto() reports
	// ENOMEM as soon as two datagrams are in flight. Wait for one to free up rather than drop:
	// giving up after a few ms lost the tail of nearly every multi-datagram video frame.
	const uint64_t deadline = millis() + waitMs;
	for(;;){
		const int n = ::sendto(sock, data, len, 0, reinterpret_cast<const sockaddr*>(&target), sizeof(target));
		if(n == (int) len){
			return true;
		}
		if(errno != ENOMEM && errno != EAGAIN && errno != EWOULDBLOCK){
			return false;
		}
		if(millis() >= deadline){
			return false;
		}
		vTaskDelay(1);
	}
}

void LinkService::sendVideoDatagrams(const Buf* buf, bool interleaveCtrl){
	// buf->data: seq u32, ts u32, w u16, h u16, JPEG. Each datagram repeats that header plus
	// frag/frags, so the plane can reassemble and drop incomplete frames.
	static constexpr size_t Chunk = LinkProto::MaxDatagramPayload - LinkProto::VideoHeaderLen;
	const uint8_t* jpeg = buf->data + 12;
	const size_t jpegLen = buf->len - 12;
	const uint16_t frags = (uint16_t) ((jpegLen + Chunk - 1) / Chunk);
	uint8_t dgram[LinkProto::HeaderLen + LinkProto::MaxDatagramPayload];
	for(uint16_t i = 0; i < frags; i++){
		const size_t off = (size_t) i * Chunk;
		const size_t n = std::min(Chunk, jpegLen - off);
		LinkProto::putHeader(dgram, LinkProto::FrameType::Video, buf->flags, LinkProto::VideoHeaderLen + n);
		uint8_t* p = dgram + LinkProto::HeaderLen;
		memcpy(p, buf->data, 12);
		LinkProto::putU16(p + 12, i);
		LinkProto::putU16(p + 14, frags);
		memcpy(p + LinkProto::VideoHeaderLen, jpeg + off, n);
		if(!sendRaw(dgram, LinkProto::HeaderLen + LinkProto::VideoHeaderLen + n, 100)){
			return; // the plane drops the incomplete frame
		}
		// A frame is a dozen datagrams now; do not hold request acks back behind all of them.
		if(interleaveCtrl){
			sendPendingCtrl();
		}
	}
}

void LinkService::sendPendingCtrl(){
	Buf* buf = nullptr;
	while(xQueueReceive(ctrlQueue, &buf, 0) == pdTRUE){
		if(buf->video){
			sendVideoDatagrams(buf, false); // a snapshot: sent whole, like any control reply
		}else{
			sendRaw(buf->data, buf->len);
		}
		freeBuf(buf);
	}
}

void LinkService::txLoop(){
	xSemaphoreTake(txWake, pdMS_TO_TICKS(100));

	auto send = [this](Buf* buf){
		if(buf->video){
			sendVideoDatagrams(buf);
		}else{
			sendRaw(buf->data, buf->len);
		}
		freeBuf(buf);
	};

	for(;;){
		Buf* buf = nullptr;
		if(xQueueReceive(ctrlQueue, &buf, 0) == pdTRUE){
			send(buf);
			continue;
		}
		if(xQueueReceive(micQueue, &buf, 0) == pdTRUE){
			send(buf);
			continue;
		}
		buf = pendingVideo.exchange(nullptr);
		if(buf != nullptr){
			send(buf);
			continue;
		}
		break;
	}
}

void LinkService::drainQueues(){
	Buf* buf = nullptr;
	while(ctrlQueue && xQueueReceive(ctrlQueue, &buf, 0) == pdTRUE){
		freeBuf(buf);
	}
	while(micQueue && xQueueReceive(micQueue, &buf, 0) == pdTRUE){
		freeBuf(buf);
	}
	freeBuf(pendingVideo.exchange(nullptr));
}

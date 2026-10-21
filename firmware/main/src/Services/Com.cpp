#include "Com.h"
#include "Link/LinkService.h"
#include "Link/LinkCredentials.h"

DEFINE_LOG(Com)

Com::Com(BLE::Server* server, bool internalStack) : AsyncEntity(10, 2*1024, CONFIG_CMF_ASYNCENTITY_THREAD_PRIORITY, CONFIG_CMF_ASYNCENTITY_CPU_CORE, internalStack){
	service = server->addService(ServiceUID);
	txChar = service->addChar(TxCharUID, ESP_GATT_CHAR_PROP_BIT_NOTIFY);
	rxChar = service->addChar(RxCharUID, ESP_GATT_CHAR_PROP_BIT_WRITE);

	txChar->setNotifRegCb([this](const esp_bd_addr_t addr){
		status = ConnStatus::Connected;
		OnConnStatus.broadcast(ConnStatus::Connected);
	});

	// Service-scoped disconnect: only fires for the peer that claimed Com's service,
	// so a phone disconnect on NUS doesn't flip Com's status while the controller is still connected.
	disconnectSub = service->addOnDisconnectCb([this](const esp_bd_addr_t){
		status = ConnStatus::Disconnected;
		OnConnStatus.broadcast(ConnStatus::Disconnected);
	});

	txBuf.reserve(BufSize);
}

Com::~Com(){
	service->removeOnDisconnectCb(disconnectSub);
}

Com::ConnStatus Com::getStatus() const{
	return status;
}

void Com::sendData(BB::State state, BB::Action::Idle action){
	BB::Action _action = {
			.idle = action
	};
	sendData(state, _action);
}

void Com::sendData(BB::State state, BB::Action::Scenario action){
	BB::Action _action = {
			.scenario = action
	};
	sendData(state, _action);
}

void Com::sendData(BB::State state, BB::Action::RC action){
	BB::Action _action = {
		.remoteControl = action
	};
	sendData(state, _action);
}

void Com::sendData(BB::State state, BB::Action action){
	sendData(state, action, 0, nullptr);
}

void Com::sendData(BB::State state, BB::Action::Idle action, size_t size, const uint8_t* data){
	BB::Action _action = {
			.idle = action
	};
	sendData(state, _action, size, data);
}

void Com::sendData(BB::State state, BB::Action::Scenario action, size_t size, const uint8_t* data){
	BB::Action _action = {
			.scenario = action
	};
	sendData(state, _action, size, data);
}

void Com::sendData(BB::State state, BB::Action::RC action, size_t size, const uint8_t* data){
	BB::Action _action = {
		.remoteControl = action
};
	sendData(state, _action, size, data);
}

void Com::sendData(BB::State state, BB::Action::AI action, size_t size, const uint8_t* data){
	BB::Action _action = {
		.ai = action
	};
	sendData(state, _action, size, data);
}

void Com::sendData(BB::State state, BB::Action action, size_t size, const uint8_t* data){
	BB bbData = {
			.state = state,
			.action = action,
			.dataSize = size
	};
	{
		std::lock_guard guard(txMut);
		txBuf.resize(sizeof(BB));
		memcpy(txBuf.data(), &bbData, sizeof(BB));

		if(txBuf.size() + size >= BufSize){
			CMF_LOG(Com, LogLevel::Error, "sendData: Data is bigger than buffer (%d >= %d)", txBuf.size() + size, BufSize);
			txBuf.clear();
			return;
		}

		txBuf.resize(txBuf.size() + size);
		memcpy(txBuf.data() + txBuf.size() - size, data, size);

		txChar->sendNotif(txBuf);
		txBuf.clear();
	}

	// Same telemetry for the AI control plane.
	if(LinkService* link = LinkService::instance()){
		link->mirrorBB(state, action, data, size);
	}
}

void Com::tick(float deltaTime) noexcept{
	if(status != ConnStatus::Connected) return;

	auto notif = rxChar->getNextWrite(portMAX_DELAY);
	if(!notif || notif->data.empty()) return;

	const auto buf = notif->data;
	if(buf.size() < sizeof(Ctrl)){
		CMF_LOG(Com, LogLevel::Warning, "Received notif with data size less than minimum");
		return;
	}

	Ctrl::Command command;
	memcpy(&command, buf.data(), sizeof(Ctrl::Command));

	if(command == Ctrl::Drive){
		if(buf.size() < sizeof(Ctrl::Command) + sizeof(DriveData)){
			CMF_LOG(Com, LogLevel::Warning, "Dropping Drive command with truncated payload (%zu bytes)", buf.size());
			return;
		}

		DriveData drive{};
		memcpy(&drive, buf.data() + sizeof(Ctrl::Command), sizeof(DriveData));

		OnDriveData.broadcast(drive);

		return;
	}

	if(command == Ctrl::Scenario){
		if(buf.size() < sizeof(Ctrl::Command) + sizeof(BB::Action::Scenario) + sizeof(ScenarioData)){
			CMF_LOG(Com, LogLevel::Warning, "Dropping Scenario command with truncated payload (%zu bytes)", buf.size());
			return;
		}

		BB::Action::Scenario scenario;
		memcpy(&scenario, buf.data() + sizeof(Ctrl::Command), sizeof(BB::Action::Scenario));

		ScenarioData scenarioData{};
		memcpy(&scenarioData, buf.data() + sizeof(Ctrl::Command) + sizeof(BB::Action::Scenario), sizeof(ScenarioData));

		OnScenario.broadcast(scenario, scenarioData);

		return;
	}

	if(command == Ctrl::LinkConfig){
		if(buf.size() < sizeof(Ctrl::Command) + sizeof(LinkConfigData)){
			CMF_LOG(Com, LogLevel::Warning, "Dropping LinkConfig command with truncated payload (%zu bytes)", buf.size());
			return;
		}

		LinkConfigData config{};
		memcpy(&config, buf.data() + sizeof(Ctrl::Command), sizeof(LinkConfigData));
		config.SSID[sizeof(config.SSID) - 1] = 0;
		config.password[sizeof(config.password) - 1] = 0;
		config.host[sizeof(config.host) - 1] = 0;

		LinkService* link = LinkService::instance();
		const uint8_t ok = link != nullptr && LinkCredentials::isValid(config);
		if(ok){
			link->configure(config);
			// Try the new settings right away (only takes effect when the robot is idle).
			link->requestWake();
		}

		// Confirmation for the provisioning tool: state Idle, a private action value, payload ok.
		BB::Action action{};
		const uint32_t raw = LinkProto::LinkConfigAckAction;
		memcpy(&action, &raw, std::min(sizeof(action), sizeof(raw)));
		sendData(BB::State::Idle, action, sizeof(ok), &ok);
		return;
	}

	if(command == Ctrl::LinkTune){
		uint8_t ok = 0;
		if(buf.size() >= sizeof(Ctrl::Command) + sizeof(LinkTuneData)){
			LinkTuneData tune{};
			memcpy(&tune, buf.data() + sizeof(Ctrl::Command), sizeof(LinkTuneData));
			if(LinkService* link = LinkService::instance()){
				link->tune(tune);
				ok = 1;
			}
		}

		BB::Action action{};
		const uint32_t raw = LinkProto::LinkTuneAckAction;
		memcpy(&action, &raw, std::min(sizeof(action), sizeof(raw)));
		sendData(BB::State::Idle, action, sizeof(ok), &ok);
		return;
	}

	if(command == Ctrl::LinkWake){
		LinkService* link = LinkService::instance();
		const uint8_t ok = link != nullptr ? link->requestWake() : 0;

		BB::Action action{};
		const uint32_t raw = LinkProto::LinkWakeAckAction;
		memcpy(&action, &raw, std::min(sizeof(action), sizeof(raw)));
		sendData(BB::State::Idle, action, sizeof(ok), &ok);
		return;
	}

	if(command == Ctrl::EnterRC){
		if(buf.size() < sizeof(Ctrl::Command) + sizeof(RCData)){
			CMF_LOG(Com, LogLevel::Warning, "Dropping EnterRC command with truncated payload (%zu bytes)", buf.size());
			return;
		}

		RCData rcData{};
		memcpy(&rcData, buf.data() + sizeof(Ctrl::Command), sizeof(RCData));

		OnRCData.broadcast(rcData);
	}

	OnCommand.broadcast(command);
}

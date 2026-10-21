#include "HomeWiFi.h"

#include <atomic>
#include <cstring>
#include <esp_event.h>
#include <esp_log.h>
#include <esp_netif.h>
#include <esp_wifi.h>

static constexpr const char* TAG = "HomeWiFi";

namespace {
	std::atomic<HomeWiFi::State> current{ HomeWiFi::State::Off };
	std::atomic<int> retries{ 0 };
	constexpr int MaxRetries = 5;
	esp_netif_t* netif = nullptr;
	bool handlersRegistered = false;

	void onWiFiEvent(void*, esp_event_base_t, int32_t id, void*){
		if(id != WIFI_EVENT_STA_DISCONNECTED || current == HomeWiFi::State::Off){
			return;
		}
		if(retries++ < MaxRetries){
			esp_wifi_connect();
			current = HomeWiFi::State::Joining;
		}else{
			ESP_LOGW(TAG, "could not join (%d tries)", MaxRetries);
			current = HomeWiFi::State::Failed;
		}
	}

	void onGotIP(void*, esp_event_base_t, int32_t, void*){
		if(current == HomeWiFi::State::Off){
			return;
		}
		retries = 0;
		current = HomeWiFi::State::Joined;
		ESP_LOGW(TAG, "joined");
	}
}

void HomeWiFi::join(const char* ssid, const char* password){
	if(netif == nullptr){
		netif = esp_netif_create_default_wifi_sta();
	}
	if(!handlersRegistered){
		esp_event_handler_instance_register(WIFI_EVENT, WIFI_EVENT_STA_DISCONNECTED, onWiFiEvent, nullptr, nullptr);
		esp_event_handler_instance_register(IP_EVENT, IP_EVENT_STA_GOT_IP, onGotIP, nullptr, nullptr);
		handlersRegistered = true;
	}

	wifi_config_t cfg{};
	strncpy((char*) cfg.sta.ssid, ssid, sizeof(cfg.sta.ssid) - 1);
	strncpy((char*) cfg.sta.password, password, sizeof(cfg.sta.password) - 1);
	cfg.sta.threshold.authmode = WIFI_AUTH_WPA2_PSK;
	cfg.sta.pmf_cfg.capable = true;

	// Keep the details out of flash
	esp_wifi_set_storage(WIFI_STORAGE_RAM);
	esp_wifi_set_mode(WIFI_MODE_APSTA);
	esp_wifi_set_config(WIFI_IF_STA, &cfg);
	memset(&cfg, 0, sizeof(cfg));

	retries = 0;
	current = State::Joining;
	esp_wifi_connect();
}

void HomeWiFi::leave(){
	if(current == State::Off){
		return;
	}
	current = State::Off;
	esp_wifi_disconnect();
	wifi_config_t forget{};
	esp_wifi_set_config(WIFI_IF_STA, &forget);
	esp_wifi_set_mode(WIFI_MODE_AP);

	// The access point followed the home network's channel; RC mode expects its own.
	wifi_config_t ap{};
	if(esp_wifi_get_config(WIFI_IF_AP, &ap) == ESP_OK && ap.ap.channel != 1){
		ap.ap.channel = 1;
		esp_wifi_set_config(WIFI_IF_AP, &ap);
	}
}

HomeWiFi::State HomeWiFi::state(){
	return current;
}

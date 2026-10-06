#include "LinkCredentials.h"
#include <nvs.h>
#include <cstring>
#include <Log/Log.h>

DEFINE_LOG(LinkCredentials)

static constexpr const char* Namespace = "BB_Link";
static constexpr const char* Key = "config";

bool LinkCredentials::load(LinkConfigData& out){
	nvs_handle_t handle;
	if(nvs_open(Namespace, NVS_READONLY, &handle) != ESP_OK){
		return false;
	}

	size_t size = sizeof(LinkConfigData);
	const esp_err_t err = nvs_get_blob(handle, Key, &out, &size);
	nvs_close(handle);

	if(err != ESP_OK || size != sizeof(LinkConfigData)){
		return false;
	}

	// Never trust stored strings to be terminated.
	out.SSID[sizeof(out.SSID) - 1] = 0;
	out.password[sizeof(out.password) - 1] = 0;
	out.host[sizeof(out.host) - 1] = 0;
	return isValid(out);
}

bool LinkCredentials::store(const LinkConfigData& data){
	nvs_handle_t handle;
	if(nvs_open(Namespace, NVS_READWRITE, &handle) != ESP_OK){
		CMF_LOG(LinkCredentials, LogLevel::Error, "nvs_open failed");
		return false;
	}

	esp_err_t err = nvs_set_blob(handle, Key, &data, sizeof(LinkConfigData));
	if(err == ESP_OK){
		err = nvs_commit(handle);
	}
	nvs_close(handle);

	if(err != ESP_OK){
		CMF_LOG(LinkCredentials, LogLevel::Error, "storing link config failed: %s", esp_err_to_name(err));
	}
	return err == ESP_OK;
}

bool LinkCredentials::isValid(const LinkConfigData& data){
	return strnlen(data.SSID, sizeof(data.SSID)) > 0 && strnlen(data.SSID, sizeof(data.SSID)) < sizeof(data.SSID)
		   && strnlen(data.host, sizeof(data.host)) > 0 && strnlen(data.host, sizeof(data.host)) < sizeof(data.host)
		   && strnlen(data.password, sizeof(data.password)) < sizeof(data.password)
		   // WPA needs 8..63 characters; empty means an open network.
		   && (strnlen(data.password, sizeof(data.password)) == 0 || strnlen(data.password, sizeof(data.password)) >= 8)
		   && data.port != 0;
}

static constexpr const char* StartingKey = "wifi_start";

void LinkCredentials::setWiFiStarting(bool starting){
	nvs_handle_t handle;
	if(nvs_open(Namespace, NVS_READWRITE, &handle) != ESP_OK){
		return;
	}
	nvs_set_u8(handle, StartingKey, starting ? 1 : 0);
	nvs_commit(handle);
	nvs_close(handle);
}

bool LinkCredentials::wasWiFiStarting(){
	nvs_handle_t handle;
	if(nvs_open(Namespace, NVS_READONLY, &handle) != ESP_OK){
		return false;
	}
	uint8_t value = 0;
	nvs_get_u8(handle, StartingKey, &value);
	nvs_close(handle);
	return value != 0;
}

static constexpr const char* TuneKey = "tune";

bool LinkCredentials::loadTune(LinkTuneData& out){
	nvs_handle_t handle;
	if(nvs_open(Namespace, NVS_READONLY, &handle) != ESP_OK){
		return false;
	}
	size_t size = sizeof(LinkTuneData);
	const esp_err_t err = nvs_get_blob(handle, TuneKey, &out, &size);
	nvs_close(handle);
	return err == ESP_OK && size == sizeof(LinkTuneData);
}

bool LinkCredentials::storeTune(const LinkTuneData& data){
	nvs_handle_t handle;
	if(nvs_open(Namespace, NVS_READWRITE, &handle) != ESP_OK){
		return false;
	}
	esp_err_t err = nvs_set_blob(handle, TuneKey, &data, sizeof(LinkTuneData));
	if(err == ESP_OK){
		err = nvs_commit(handle);
	}
	nvs_close(handle);
	return err == ESP_OK;
}

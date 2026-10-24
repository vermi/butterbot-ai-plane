#include "Ota.h"
#include "LinkCredentials.h"
#include "Util/AiRestart.h"
#include "Pins.hpp"
#include "Services/ShutdownService.h"
#include <driver/gpio.h>
#include <algorithm>
#include <cstdio>
#include <cstring>
#include <esp_app_desc.h>
#include <esp_attr.h>
#include <esp_event.h>
#include <esp_heap_caps.h>
#include <esp_http_client.h>
#include <esp_netif.h>
#include <esp_ota_ops.h>
#include <esp_system.h>
#include <esp_wifi.h>
#include <freertos/FreeRTOS.h>
#include <freertos/event_groups.h>
#include <freertos/task.h>
#include <mbedtls/sha256.h>
#include <Log/Log.h>

DEFINE_LOG(Ota)

namespace {
	constexpr uint32_t Magic = 0x0DA7E0DA;
	constexpr uint32_t JoinTimeoutMs = 30000;
	// Update mode never runs longer than this: whatever happened, it restarts into AI mode.
	constexpr uint32_t UpdateModeMaxMs = 6 * 60 * 1000;
	// A press this long switches the robot off (nothing else listens to the button meanwhile).
	constexpr uint32_t OffPressMs = 1500;

	/** The update to run after the restart. Survives a software restart, not a power cycle. */
	struct Job {
		uint32_t magic;
		uint32_t size;
		char url[192];
		char sha256[65];
	};
	RTC_NOINIT_ATTR Job pending;

	EventGroupHandle_t wifiEvents = nullptr;
	constexpr EventBits_t GotIP = BIT0;

	void onWifiEvent(void*, esp_event_base_t base, int32_t id, void* data){
		if(base == WIFI_EVENT && id == WIFI_EVENT_STA_DISCONNECTED){
			CMF_LOG(Ota, LogLevel::Warning, "Wi-Fi lost (reason %u)", static_cast<wifi_event_sta_disconnected_t*>(data)->reason);
			esp_wifi_connect(); // and again until it works (or update mode gives up)
		}else if(base == WIFI_EVENT && id == WIFI_EVENT_STA_START){
			esp_wifi_connect();
		}else if(base == IP_EVENT && id == IP_EVENT_STA_GOT_IP){
			xEventGroupSetBits(wifiEvents, GotIP);
		}
	}

	/** Join the home Wi-Fi the link uses. Returns nullptr on success, else why it failed. */
	const char* joinWifi(){
		LinkConfigData cfg{};
		if(!LinkCredentials::load(cfg) || !LinkCredentials::isValid(cfg)){
			return "no Wi-Fi settings";
		}
		wifiEvents = xEventGroupCreate();
		esp_netif_init();
		esp_event_loop_create_default(); // may exist already: fine
		esp_netif_create_default_wifi_sta();
		wifi_init_config_t init = WIFI_INIT_CONFIG_DEFAULT();
		// AI mode keeps Wi-Fi's buffers to the bare minimum (its RAM goes to everything else);
		// update mode has the RAM, and a download needs them: with two, frames get dropped.
		init.static_rx_buf_num = 10;
		init.dynamic_rx_buf_num = 32;
		init.rx_ba_win = 16;
		init.dynamic_tx_buf_num = 32;
		if(esp_wifi_init(&init) != ESP_OK){
			return "Wi-Fi would not start";
		}
		esp_event_handler_register(WIFI_EVENT, ESP_EVENT_ANY_ID, onWifiEvent, nullptr);
		esp_event_handler_register(IP_EVENT, IP_EVENT_STA_GOT_IP, onWifiEvent, nullptr);
		esp_wifi_set_storage(WIFI_STORAGE_RAM);
		esp_wifi_set_mode(WIFI_MODE_STA);
		wifi_config_t wc = {};
		memcpy(wc.sta.ssid, cfg.SSID, strnlen(cfg.SSID, sizeof(cfg.SSID)));
		memcpy(wc.sta.password, cfg.password, strnlen(cfg.password, sizeof(cfg.password)));
		wc.sta.channel = LinkCredentials::loadChannel(); // a hint; 0 scans all channels
		esp_wifi_set_config(WIFI_IF_STA, &wc);
		if(esp_wifi_start() != ESP_OK){
			return "Wi-Fi would not start";
		}
		esp_wifi_set_ps(WIFI_PS_NONE);
		if(!(xEventGroupWaitBits(wifiEvents, GotIP, pdFALSE, pdTRUE, pdMS_TO_TICKS(JoinTimeoutMs)) & GotIP)){
			return "could not join Wi-Fi";
		}
		return nullptr;
	}

	/** Returns nullptr on success, else why it failed. */
	const char* download(const Job& job){
		const esp_partition_t* slot = esp_ota_get_next_update_partition(nullptr);
		if(slot == nullptr){
			return "no update slot (flash the OTA partition table over USB first)";
		}
		if(job.size == 0 || job.size > slot->size){
			return "image does not fit the update slot";
		}
		// Erase first (it takes a while), so the download then runs at full speed.
		const uint32_t erasing = xTaskGetTickCount();
		esp_ota_handle_t ota = 0;
		if(esp_ota_begin(slot, job.size, &ota) != ESP_OK){
			return "could not erase the update slot";
		}
		CMF_LOG(Ota, LogLevel::Warning, "erased %s in %lu ms", slot->label, (unsigned long) pdTICKS_TO_MS(xTaskGetTickCount() - erasing));
		const uint32_t downloading = xTaskGetTickCount();
		uint32_t logged = 0;

		esp_http_client_config_t cfg = {};
		cfg.url = job.url;
		cfg.timeout_ms = 15000;
		cfg.buffer_size = 4096;
		esp_http_client_handle_t http = esp_http_client_init(&cfg);
		constexpr size_t BufSize = 16384;
		// Flash is written with the cache off: the buffer must be in internal RAM.
		auto* buf = static_cast<char*>(heap_caps_malloc(BufSize, MALLOC_CAP_INTERNAL | MALLOC_CAP_8BIT));
		mbedtls_sha256_context sha;
		mbedtls_sha256_init(&sha);
		mbedtls_sha256_starts(&sha, 0);
		const char* fail = nullptr;
		uint32_t done = 0;

		if(http == nullptr || buf == nullptr){
			fail = "out of memory";
		}else if(esp_http_client_open(http, 0) != ESP_OK){
			fail = "could not reach the plane's update server";
		}else if(const int64_t len = esp_http_client_fetch_headers(http); len >= 0 && len != job.size){
			fail = "the server sent a different size";
		}else if(esp_http_client_get_status_code(http) != 200){
			fail = "the server refused the download";
		}else{
			while(done < job.size){
				const int n = esp_http_client_read(http, buf, std::min<uint32_t>(BufSize, job.size - done));
				if(n <= 0){
					fail = "the download broke off";
					break;
				}
				mbedtls_sha256_update(&sha, reinterpret_cast<const unsigned char*>(buf), n);
				if(esp_ota_write(ota, buf, n) != ESP_OK){
					fail = "writing flash failed";
					break;
				}
				done += n;
				if(done - logged >= job.size / 10){
					logged = done;
					CMF_LOG(Ota, LogLevel::Warning, "%lu%% after %lu ms", (unsigned long) ((uint64_t) done * 100 / job.size),
							(unsigned long) pdTICKS_TO_MS(xTaskGetTickCount() - downloading));
				}
			}
		}
		if(fail == nullptr){
			unsigned char digest[32];
			mbedtls_sha256_finish(&sha, digest);
			char hex[65];
			for(int i = 0; i < 32; i++){
				snprintf(hex + i * 2, 3, "%02x", digest[i]);
			}
			if(strcasecmp(hex, job.sha256) != 0){
				fail = "the image's checksum does not match";
			}
		}
		mbedtls_sha256_free(&sha);
		if(http){
			esp_http_client_close(http);
			esp_http_client_cleanup(http);
		}
		free(buf);

		if(fail != nullptr){
			esp_ota_abort(ota);
			return fail;
		}
		if(esp_ota_end(ota) != ESP_OK){
			return "the image is not valid firmware";
		}
		if(esp_ota_set_boot_partition(slot) != ESP_OK){
			return "could not select the new firmware";
		}
		CMF_LOG(Ota, LogLevel::Warning, "new firmware in %s (%lu bytes)", slot->label, (unsigned long) done);
		return nullptr;
	}

	/** Tell the plane how it went: a GET of the image's URL with ?result=ok or ?result=failed. */
	void report(const Job& job, const char* fail){
		char url[sizeof(job.url) + 160];
		int n = snprintf(url, sizeof(url), "%s?result=%s", job.url, fail ? "failed&error=" : "ok");
		for(const char* c = fail; c && *c && n < (int) sizeof(url) - 4; c++){
			const bool plain = isalnum((unsigned char) *c) || *c == '-' || *c == '.' || *c == '_';
			n += plain ? snprintf(url + n, sizeof(url) - n, "%c", *c) : snprintf(url + n, sizeof(url) - n, "%%%02X", (unsigned char) *c);
		}
		esp_http_client_config_t cfg = {};
		cfg.url = url;
		cfg.timeout_ms = 5000;
		if(esp_http_client_handle_t http = esp_http_client_init(&cfg)){
			esp_http_client_perform(http);
			esp_http_client_cleanup(http);
		}
	}

	void updateTask(void* arg){
		const Job job = *static_cast<Job*>(arg);
		CMF_LOG(Ota, LogLevel::Warning, "update mode: %lu bytes from %s", (unsigned long) job.size, job.url);
		const char* fail = joinWifi();
		if(fail == nullptr){
			fail = download(job);
			report(job, fail);
		}
		if(fail){
			CMF_LOG(Ota, LogLevel::Error, "update failed: %s", fail);
		}
		esp_wifi_stop();
		// Back to AI mode, on the new firmware or the one it had.
		restartIntoAiMode();
	}
}

void Ota::restartIntoUpdate(const std::string& url, uint32_t size, const std::string& sha256){
	pending = {};
	pending.size = size;
	strncpy(pending.url, url.c_str(), sizeof(pending.url) - 1);
	strncpy(pending.sha256, sha256.c_str(), sizeof(pending.sha256) - 1);
	pending.magic = Magic;
	esp_restart();
}

void Ota::runPending(){
	if(esp_reset_reason() != ESP_RST_SW || pending.magic != Magic){
		pending.magic = 0;
		return;
	}
	// Once only: a crash in update mode boots normally.
	static Job job;
	job = pending;
	pending.magic = 0;
	job.url[sizeof(job.url) - 1] = 0;
	job.sha256[sizeof(job.sha256) - 1] = 0;
	// Its own task: the caller's stack is small. Internal-RAM stack: flash is written from it.
	if(xTaskCreatePinnedToCore(updateTask, "update", 8192, &job, 5, nullptr, 0) != pdPASS){
		restartIntoAiMode();
	}
	// Meanwhile: the way out if it gets stuck, and the button (a long press switches off).
	static constexpr gpio_config_t btn = { .pin_bit_mask = 1ULL << PIN_BTN, .mode = GPIO_MODE_INPUT };
	gpio_config(&btn);
	const uint32_t started = xTaskGetTickCount();
	uint32_t pressedFor = 0;
	while(true){
		vTaskDelay(pdMS_TO_TICKS(100));
		pressedFor = gpio_get_level(static_cast<gpio_num_t>(PIN_BTN)) ? pressedFor + 100 : 0;
		if(pressedFor >= OffPressMs){
			CMF_LOG(Ota, LogLevel::Warning, "button: switching off");
			ShutdownService::PowerOff();
		}
		if(pdTICKS_TO_MS(xTaskGetTickCount() - started) > UpdateModeMaxMs){
			CMF_LOG(Ota, LogLevel::Error, "update mode took too long, giving up");
			restartIntoAiMode();
		}
	}
}

void Ota::confirm(){
	const esp_partition_t* running = esp_ota_get_running_partition();
	esp_ota_img_states_t state;
	if(running && esp_ota_get_state_partition(running, &state) == ESP_OK && state == ESP_OTA_IMG_PENDING_VERIFY){
		esp_ota_mark_app_valid_cancel_rollback();
		CMF_LOG(Ota, LogLevel::Warning, "new firmware reached the plane: keeping it");
	}
}

const char* Ota::build(){
	// Not esp_app_get_elf_sha256(): it stops at CONFIG_APP_RETRIEVE_LEN_ELF_SHA characters.
	static char id[17] = {};
	if(id[0] == 0){
		const uint8_t* sha = esp_app_get_description()->app_elf_sha256;
		for(int i = 0; i < 8; i++){
			snprintf(id + i * 2, 3, "%02x", sha[i]);
		}
	}
	return id;
}

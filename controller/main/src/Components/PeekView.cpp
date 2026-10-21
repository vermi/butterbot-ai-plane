#include "PeekView.h"

#include <cstring>
#include <esp_heap_caps.h>
#include <lwip/sockets.h>
#include <Util/stdafx.h>
#include "Fonts/font.hpp"
#include "Util/HomeWiFi.h"
#include <esp_log.h>

PeekView::PeekView(lv_obj_t* parent) : LVObject(parent){
	theme = Application::getApp()->getService<ThemeService>();
	openedAt = millis();

	lv_obj_set_size(*this, 128, 120);
	lv_obj_set_pos(*this, 0, 8);
	lv_obj_remove_flag(*this, LV_OBJ_FLAG_SCROLLABLE);
	lv_obj_set_style_bg_color(*this, theme->getTertiaryColor(), 0);
	lv_obj_set_style_bg_opa(*this, LV_OPA_COVER, 0);
	lv_obj_set_style_pad_all(*this, 0, 0);
	lv_obj_set_style_border_width(*this, 0, 0);
	lv_obj_set_style_radius(*this, 0, 0);

	pixels = (uint8_t*) heap_caps_malloc(W * H * 2, MALLOC_CAP_SPIRAM);
	if(pixels != nullptr){
		memset(pixels, 0, W * H * 2);
	}
	dsc.header.magic = LV_IMAGE_HEADER_MAGIC;
	dsc.header.cf = LV_COLOR_FORMAT_RGB565;
	dsc.header.w = W;
	dsc.header.h = H;
	dsc.header.stride = W * 2;
	dsc.data_size = W * H * 2;
	dsc.data = pixels;

	img = lv_image_create(*this);
	lv_obj_set_pos(img, 0, 2);
	lv_obj_add_flag(img, LV_OBJ_FLAG_HIDDEN);
	if(pixels != nullptr){
		lv_image_set_src(img, &dsc);
	}

	status = lv_label_create(*this);
	lv_obj_set_style_text_font(status, &lv_font_butter, 0);
	lv_obj_set_style_text_color(status, theme->getPrimaryColor(), 0);
	lv_obj_set_style_text_align(status, LV_TEXT_ALIGN_CENTER, 0);
	lv_obj_set_width(status, 128);
	lv_obj_set_pos(status, 0, 44);
	lv_label_set_text(status, "Getting Wi-Fi...");

	caption = lv_label_create(*this);
	lv_obj_set_style_text_font(caption, &lv_font_butter, 0);
	lv_obj_set_style_text_color(caption, theme->getPrimaryColor(), 0);
	lv_obj_set_style_text_align(caption, LV_TEXT_ALIGN_CENTER, 0);
	lv_label_set_long_mode(caption, LV_LABEL_LONG_SCROLL_CIRCULAR);
	lv_obj_set_size(caption, 124, 8);
	lv_obj_set_pos(caption, 2, H + 6);
	lv_label_set_text(caption, "");

	feed = newObject<Feed>();
}

PeekView::~PeekView(){
	if(sock >= 0){
		close(sock);
	}
	HomeWiFi::leave();
	if(pixels != nullptr){
		heap_caps_free(pixels);
	}
}

void PeekView::onWiFi(const std::vector<uint8_t>& data){
	if(data.size() != sizeof(PeekWiFiData) || planeIp != 0){
		return;
	}
	PeekWiFiData d{};
	memcpy(&d, data.data(), sizeof(d));
	d.ssid[sizeof(d.ssid) - 1] = 0;
	d.password[sizeof(d.password) - 1] = 0;
	planeIp = d.planeIp;
	ESP_LOGW("PeekView", "got Wi-Fi details, joining");
	HomeWiFi::join(d.ssid, d.password);
	memset(&d, 0, sizeof(d));
	setStatus("Joining Wi-Fi...");
}

void PeekView::onCaption(const std::vector<uint8_t>& data){
	const PeekCaptionData d = PeekCaptionData::deserialize(data.data(), data.size());
	if(strcmp(lv_label_get_text(caption), d.text.c_str()) != 0){
		lv_label_set_text(caption, d.text.c_str());
	}
}

void PeekView::loop(){
	const uint64_t now = millis();
	if(planeIp == 0){
		if(now - openedAt > WiFiDetailsWaitMs){
			setStatus("The robot has no\nWi-Fi details");
		}
		return;
	}

	switch(HomeWiFi::state()){
		case HomeWiFi::State::Failed:
			setStatus("Could not join\nthe Wi-Fi");
			return;
		case HomeWiFi::State::Joining:
			if(now - openedAt > JoinWaitMs){
				setStatus("Could not join\nthe Wi-Fi");
			}
			return;
		case HomeWiFi::State::Joined:
			if(joinedAt == 0){
				joinedAt = now;
				setStatus("Looking...");
			}
			break;
		case HomeWiFi::State::Off:
			return;
	}

	if(now - lastHello >= HelloMs){
		lastHello = now;
		sayHello();
	}
	if(now - lastLog >= 2000){
		lastLog = now;
		ESP_LOGI("PeekView", "received %lu bytes, decoded %lu frames", (unsigned long) feed->bytesIn.load(),
				 (unsigned long) feed->framesDecoded.load());
	}

	feed->nextFrame([this](const uint16_t* frame){
		if(pixels == nullptr){
			return;
		}
		memcpy(pixels, frame, W * H * 2);
		if(!gotFrame){
			gotFrame = true;
			lv_obj_add_flag(status, LV_OBJ_FLAG_HIDDEN);
			lv_obj_remove_flag(img, LV_OBJ_FLAG_HIDDEN);
		}
		lv_obj_invalidate(img);
	});

	if(!gotFrame && now - joinedAt > 6000){
		setStatus("No picture from\nthe computer");
	}
}

void PeekView::sayHello(){
	if(sock < 0){
		sock = socket(AF_INET, SOCK_DGRAM, IPPROTO_IP);
		if(sock < 0){
			return;
		}
	}
	sockaddr_in to{};
	to.sin_family = AF_INET;
	to.sin_port = htons(PeekWiFiData::PlanePort);
	to.sin_addr.s_addr = planeIp;
	static constexpr char Hello[] = "BBPEEK";
	sendto(sock, Hello, sizeof(Hello) - 1, MSG_DONTWAIT, (const sockaddr*) &to, sizeof(to));
}

void PeekView::setStatus(const char* text){
	if(strcmp(lv_label_get_text(status), text) != 0){
		lv_label_set_text(status, text);
	}
}

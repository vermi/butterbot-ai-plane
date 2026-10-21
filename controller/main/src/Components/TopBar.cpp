#include "TopBar.h"
#include <LV_Interface/LVGL.h>
#include "BBData.h"
#include "Fonts/font.hpp"
#include <cstdio>
#include <cstring>
#include <Util/stdafx.h>

TopBar::TopBar(lv_obj_t* parent) : LVObject(parent){
	const auto app = Application::getApp();
	theme = app->getService<ThemeService>();
	robotState = app->getService<RobotState>();

	buildUI();

	com = app->getService<Com>();

	com->onData.bind(app->getService<LVGL>(), [this](const BB::State state, const BB::Action action, const std::vector<uint8_t>& data) {
		if(state == BB::State::Idle && action.idle == BB::Action::Idle::ShutUp){
			updateShutUpEl();
		} else if(state == BB::State::AI && action.ai == BB::Action::AI::Status){
			updateAi(data);
		}
	});
	com->onConnStatus.bind(app->getService<LVGL>(), [this](Com::ConnStatus status) {
		// The robot reports AI mode again when it reconnects.
		if(status == Com::ConnStatus::Disconnected){
			robotState->setAi({});
			robotState->clearHomeNews();
			showAi();
		}
	});
}

TopBar::~TopBar(){
	//This will unbind every other UI-related registration of Com's event, since LVGL is the only Object in the UI system.
	//However, TopBar will not be dynamically constructed. It always exists with it's parent screen.
	com->onData.unbind(Application::getApp()->getService<LVGL>());
}

void TopBar::updateAi(const std::vector<uint8_t>& data) const{
	if(data.size() != sizeof(AIStatusData)){
		return;
	}
	robotState->setAi(*(const AIStatusData*) data.data());
	showAi();
}

// What AI mode is up to, in the middle of the bar: "AI" while it listens for its
// name, "AI CHAT" in a conversation, "AI ..." while thinking, "FOCUS 42" in focus mode, "AI -"
// with no agent running. Nothing outside AI mode.
void TopBar::showAi() const{
	using M = AIStatusData::Mode;
	char text[16] = "";
	switch(robotState->getAiMode()){
		case M::Off: break;
		case M::Listening: strcpy(text, "AI"); break;
		case M::Attentive: strcpy(text, "AI CHAT"); break;
		case M::Thinking: strcpy(text, "AI ..."); break;
		case M::NoBrain: strcpy(text, "AI -"); break;
		case M::Focus: {
			const uint8_t m = robotState->getAiFocusMinutes();
			if(m == 0 || m == 255){
				strcpy(text, "FOCUS");
			}else{
				snprintf(text, sizeof(text), "FOCUS %u", m);
			}
			break;
		}
	}
	lv_label_set_text(aiLabel, text);
}

void TopBar::updateShutUpEl() const{
	shutupEl->setMutedStatus(robotState->isMuted());
}

void TopBar::setRobotElementsVisible(const bool visible) const{
	const lv_opa_t opa = visible ? LV_OPA_COVER : LV_OPA_TRANSP;
	lv_obj_set_style_opa(*shutupEl, opa, 0);
	lv_obj_set_style_opa(aiLabel, opa, 0);
}

void TopBar::loop(){
	if(millis() - lastClock < 1000){
		return;
	}
	lastClock = millis();
	char text[8] = "";
	if(tm t{}; robotState->getClock(t)){
		snprintf(text, sizeof(text), "%d:%02d", t.tm_hour, t.tm_min);
	}
	if(strcmp(lv_label_get_text(clockLabel), text) != 0){
		lv_label_set_text(clockLabel, text);
	}
}

void TopBar::buildUI(){
	const lv_color_t color = theme->getPrimaryColor();

	// The robot's clock on the left, AI mode's status in the middle, the mute icon (only while
	// muted) and the signal on the right. The batteries are on the home screen, as percentages.
	clockLabel = lv_label_create(*this);
	lv_obj_set_style_text_font(clockLabel, &lv_font_butter, 0);
	lv_obj_set_style_text_color(clockLabel, theme->getTertiaryColor(), 0);
	lv_label_set_long_mode(clockLabel, LV_LABEL_LONG_CLIP);
	lv_obj_set_size(clockLabel, 24, 8);
	lv_obj_set_pos(clockLabel, 2, 0);
	lv_label_set_text(clockLabel, "");

	shutupEl = new ShutUpElement(*this, robotState->isMuted());
	signalEl = new SignalElement(*this, 2);
	lv_obj_set_pos(*shutupEl, 104, 1);
	lv_obj_set_pos(*signalEl, 116, 1);

	aiLabel = lv_label_create(*this);
	lv_obj_set_style_text_font(aiLabel, &lv_font_butter, 0);
	lv_obj_set_style_text_color(aiLabel, theme->getTertiaryColor(), 0);
	lv_obj_set_style_text_align(aiLabel, LV_TEXT_ALIGN_CENTER, 0);
	lv_label_set_long_mode(aiLabel, LV_LABEL_LONG_CLIP);
	lv_obj_set_size(aiLabel, 72, 8);
	lv_obj_set_pos(aiLabel, 28, 0);
	showAi();

	lv_obj_set_size(*this, lv_pct(100), 8);

	lv_obj_set_style_bg_color(*this, color, 0);
	lv_obj_set_style_bg_color(*signalEl, color, 0);
	lv_obj_set_style_bg_color(*shutupEl, color, 0);
}

#include "HomeDash.h"

#include <algorithm>
#include <cstdio>
#include <cstring>
#include <ctime>
#include <Util/stdafx.h>
#include "Fonts/font.hpp"

namespace {
	// Rows, from the top of the 120x112 area under the top bar.
	constexpr int32_t Width = 120;
	constexpr int32_t ClockY = -1; // the clock font's digits are 33 px tall
	constexpr int32_t DateY = 32;
	constexpr int32_t BotY = 43;
	constexpr int32_t CtrlY = 56;
	constexpr int32_t BarGap = 8; // from a battery row to its bar
	constexpr int32_t ModeY = 70;
	constexpr int32_t NextY = 80;
	constexpr int32_t SeenY = 90;
	constexpr int32_t SaidY = 100;

	constexpr const char* Weekdays[] = { "Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday" };
	constexpr const char* Months[] = { "January", "February", "March", "April", "May", "June", "July", "August",
									   "September", "October", "November", "December" };

	constexpr const char* Hints = "AI mode is off\nHold Override: drive\nPress the stick: menu";
}

HomeDash::HomeDash(lv_obj_t* parent) : LVObject(parent){
	const auto app = Application::getApp();
	theme = app->getService<ThemeService>();
	robotState = app->getService<RobotState>();
	battery = app->getService<Battery>();
	buildUI();
	refresh();
}

void HomeDash::loop(){
	if(lv_obj_has_flag(*this, LV_OBJ_FLAG_HIDDEN) || millis() - lastRefresh < RefreshMs){
		return;
	}
	refresh();
}

void HomeDash::refresh(){
	lastRefresh = millis();
	const HomeInfoData home = robotState->getHome();
	const uint64_t homeAt = robotState->getHomeAt();
	showClock(home, homeAt);
	showBatteries(home, homeAt);
	showAi(home, homeAt);
}

// The robot's clock (set from the phone or, in AI mode, the laptop), kept running here since it
// was sent.
void HomeDash::showClock(const HomeInfoData& home, const uint64_t homeAt){
	if(homeAt == 0 || !home.clockSet){
		lv_obj_add_flag(clock, LV_OBJ_FLAG_HIDDEN);
		setText(date, homeAt == 0 ? "" : "The clock is not set yet");
		return;
	}
	tm t{};
	t.tm_year = home.year - 1900;
	t.tm_mon = home.month - 1;
	t.tm_mday = home.day;
	t.tm_hour = home.hour;
	t.tm_min = home.minute;
	t.tm_sec = home.second;
	// No time zone is set here, so this round trip only does the calendar arithmetic.
	const time_t now = mktime(&t) + (time_t) ((millis() - homeAt) / 1000);
	localtime_r(&now, &t);

	char text[32];
	snprintf(text, sizeof(text), "%d:%02d", t.tm_hour, t.tm_min);
	setText(clock, text);
	lv_obj_remove_flag(clock, LV_OBJ_FLAG_HIDDEN);
	snprintf(text, sizeof(text), "%s %d %s", Weekdays[t.tm_wday % 7], t.tm_mday, Months[t.tm_mon % 12]);
	setText(date, text);
}

void HomeDash::showBatteries(const HomeInfoData& home, const uint64_t homeAt){
	char text[24];
	if(homeAt == 0){
		setText(botPct, "...");
		setBar(botBar, 0);
	}else{
		switch(robotState->getChargeState()){
			case ChargeStatus::Charging: snprintf(text, sizeof(text), "charging %u%%", home.botPercent); break;
			case ChargeStatus::Full: snprintf(text, sizeof(text), "full"); break;
			default: snprintf(text, sizeof(text), "%u%%", home.botPercent); break;
		}
		setText(botPct, text);
		setBar(botBar, robotState->getChargeState() == ChargeStatus::Full ? 100 : home.botPercent);
	}

	const uint8_t own = battery ? battery->getPerc() : 0;
	snprintf(text, sizeof(text), "%u%%", own);
	setText(ctrlPct, text);
	setBar(ctrlBar, own);
}

void HomeDash::showAi(const HomeInfoData& home, const uint64_t homeAt){
	const bool ai = robotState->isAi();
	lv_obj_update_flag(hints, LV_OBJ_FLAG_HIDDEN, ai);
	for(lv_obj_t* l : { mode, activity, next, seen, seenAge, said }){
		lv_obj_update_flag(l, LV_OBJ_FLAG_HIDDEN, !ai);
	}
	if(!ai){
		return;
	}

	using M = AIStatusData::Mode;
	char text[48];
	switch(robotState->getAiMode()){
		case M::Listening: snprintf(text, sizeof(text), "Listening"); break;
		case M::Attentive: snprintf(text, sizeof(text), "Chatting"); break;
		case M::Thinking: snprintf(text, sizeof(text), "Thinking..."); break;
		case M::NoBrain: snprintf(text, sizeof(text), "No agent running"); break;
		case M::Focus: {
			const uint8_t m = robotState->getAiFocusMinutes();
			if(m == 0 || m == 255){
				snprintf(text, sizeof(text), "Focus mode");
			}else{
				snprintf(text, sizeof(text), "Focus, %u min left", m);
			}
			break;
		}
		default: text[0] = 0; break;
	}
	setText(mode, text);

	using A = AIStatusData::Activity;
	switch(robotState->getAiActivity()){
		case A::Quiet: setText(activity, "quiet"); break;
		case A::Calm: setText(activity, "calm"); break;
		case A::Normal: setText(activity, "normal"); break;
		case A::Lively: setText(activity, "lively"); break;
	}

	const std::string nextText = home.next.empty() ? "No reminders" : "Next " + home.next;
	setText(next, nextText.c_str());

	setText(seen, home.seen.c_str());
	text[0] = 0;
	if(!home.seen.empty()){
		const uint64_t secs = home.seenSecs + (millis() - homeAt) / 1000;
		if(secs < 60){
			snprintf(text, sizeof(text), "now");
		}else if(secs < 3600){
			snprintf(text, sizeof(text), "%llum", secs / 60);
		}else if(secs < 86400){
			snprintf(text, sizeof(text), "%lluh", secs / 3600);
		}else{
			snprintf(text, sizeof(text), "%llud", secs / 86400);
		}
	}
	setText(seenAge, text);

	const std::string saidText = home.said.empty() ? "" : "\"" + home.said + "\"";
	setText(said, saidText.c_str());
}

void HomeDash::buildUI(){
	// Kept out of the parent's flex flow and scroll (same pattern as GuideElement)
	lv_obj_add_flag(*this, LV_OBJ_FLAG_IGNORE_LAYOUT);
	lv_obj_add_flag(*this, LV_OBJ_FLAG_FLOATING);
	lv_obj_remove_flag(*this, LV_OBJ_FLAG_SCROLLABLE);
	lv_obj_set_size(*this, Width, 112);
	lv_obj_set_pos(*this, 0, 0);
	lv_obj_set_style_bg_opa(*this, LV_OPA_TRANSP, 0);

	const lv_color_t prim = theme->getPrimaryColor();
	const lv_color_t second = theme->getSecondaryColor();

	clock = addLabel(0, ClockY, Width, LV_TEXT_ALIGN_CENTER, prim);
	lv_obj_set_style_text_font(clock, &lv_font_butter_clock, 0);
	lv_obj_set_style_text_letter_space(clock, 2, 0);
	lv_obj_set_height(clock, 33);
	date = addLabel(0, DateY, Width, LV_TEXT_ALIGN_CENTER, prim);

	lv_label_set_text_static(addLabel(0, BotY, 60, LV_TEXT_ALIGN_LEFT, second), "Butter Bot");
	botPct = addLabel(50, BotY, 70, LV_TEXT_ALIGN_RIGHT, prim);
	botBar = addBar(BotY + BarGap);
	lv_label_set_text_static(addLabel(0, CtrlY, 60, LV_TEXT_ALIGN_LEFT, second), "Remote");
	ctrlPct = addLabel(50, CtrlY, 70, LV_TEXT_ALIGN_RIGHT, prim);
	ctrlBar = addBar(CtrlY + BarGap);

	mode = addLabel(0, ModeY, 90, LV_TEXT_ALIGN_LEFT, prim);
	activity = addLabel(80, ModeY, 40, LV_TEXT_ALIGN_RIGHT, second);
	next = addLabel(0, NextY, Width, LV_TEXT_ALIGN_LEFT, prim);
	lv_label_set_long_mode(next, LV_LABEL_LONG_DOT);
	seen = addLabel(0, SeenY, 100, LV_TEXT_ALIGN_LEFT, prim);
	lv_label_set_long_mode(seen, LV_LABEL_LONG_DOT);
	seenAge = addLabel(100, SeenY, 20, LV_TEXT_ALIGN_RIGHT, second);
	said = addLabel(0, SaidY, Width, LV_TEXT_ALIGN_LEFT, second);
	lv_label_set_long_mode(said, LV_LABEL_LONG_SCROLL_CIRCULAR);

	hints = addLabel(0, ModeY, Width, LV_TEXT_ALIGN_LEFT, second);
	lv_obj_set_height(hints, LV_SIZE_CONTENT);
	lv_label_set_long_mode(hints, LV_LABEL_LONG_WRAP);
	lv_obj_set_style_text_line_space(hints, 3, 0);
	lv_label_set_text_static(hints, Hints);
}

lv_obj_t* HomeDash::addLabel(const int32_t x, const int32_t y, const int32_t w, const lv_text_align_t align, const lv_color_t color){
	lv_obj_t* l = lv_label_create(*this);
	lv_obj_set_style_text_font(l, &lv_font_butter, 0);
	lv_obj_set_style_text_color(l, color, 0);
	lv_obj_set_style_text_align(l, align, 0);
	lv_label_set_long_mode(l, LV_LABEL_LONG_CLIP);
	lv_obj_set_size(l, w, 8);
	lv_obj_set_pos(l, x, y);
	lv_label_set_text_static(l, "");
	return l;
}

lv_obj_t* HomeDash::addBar(const int32_t y){
	lv_obj_t* b = lv_bar_create(*this);
	lv_obj_set_size(b, Width, 2);
	lv_obj_set_pos(b, 0, y);
	lv_bar_set_range(b, 0, 100);
	lv_obj_set_style_radius(b, 0, LV_PART_MAIN);
	lv_obj_set_style_radius(b, 0, LV_PART_INDICATOR);
	lv_obj_set_style_bg_color(b, theme->getSecondaryColor(), LV_PART_MAIN);
	lv_obj_set_style_bg_opa(b, LV_OPA_COVER, LV_PART_MAIN);
	lv_obj_set_style_bg_color(b, theme->getPrimaryColor(), LV_PART_INDICATOR);
	lv_obj_set_style_bg_opa(b, LV_OPA_COVER, LV_PART_INDICATOR);
	lv_obj_set_style_anim_duration(b, 0, LV_PART_MAIN);
	return b;
}

void HomeDash::setText(lv_obj_t* label, const char* text){
	const char* shown = lv_label_get_text(label);
	if(shown != nullptr && strcmp(shown, text) == 0){
		return;
	}
	lv_label_set_text(label, text);
}

void HomeDash::setBar(lv_obj_t* bar, const uint8_t percent){
	lv_bar_set_value(bar, std::min<uint8_t>(percent, 100), LV_ANIM_OFF);
}

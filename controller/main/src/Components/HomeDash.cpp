#include "HomeDash.h"

#include <algorithm>
#include <cstdio>
#include <cstring>
#include <Util/stdafx.h>
#include "Fonts/font.hpp"

namespace {
	// The 120x112 area under the top bar: the face top left, a column beside it, lines below.
	constexpr int32_t Width = 120;
	constexpr int32_t ColX = 64;
	constexpr int32_t ColW = Width - ColX;
	constexpr int32_t MoodY = 2;
	constexpr int32_t ModeY = 12;
	constexpr int32_t ActivityY = 22;
	constexpr int32_t BotY = 34;
	constexpr int32_t CtrlY = 46;
	constexpr int32_t BarGap = 8; // from a battery line to its bar
	constexpr int32_t NextY = 65;
	constexpr int32_t SeenY = 75;
	constexpr int32_t SaidY = 86;
	constexpr int32_t PlayingY = 97;

	// The plane's emotions (AIStatusData::emotion).
	constexpr const char* Emotions[] = { "content", "happy", "excited", "curious", "playful", "calm", "sleepy",
										 "bored", "sad", "grumpy", "surprised", "worried" };
	constexpr const char* Activities[] = { "quiet", "calm", "normal", "lively" };

	constexpr const char* Hints = "Hold Override: drive\nPress the stick: menu\nSummon: listen, hold: come";
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
	if(lv_obj_has_flag(*this, LV_OBJ_FLAG_HIDDEN)){
		return;
	}
	face->loop();
	if(millis() - lastRefresh >= RefreshMs){
		refresh();
	}
}

void HomeDash::showActivity(const AIStatusData::Activity level){
	activityShown = level;
	activityShownUntil = millis() + ActivityShowMs;
	refresh();
}

void HomeDash::refresh(){
	lastRefresh = millis();
	const HomeInfoData home = robotState->getHome();
	const uint64_t homeAt = robotState->getHomeAt();
	showBatteries(home, homeAt);
	showAi(home, homeAt);

	const std::string music = home.playing.empty() ? "" : "Playing: " + home.playing;
	setText(playing, music.c_str());
}

void HomeDash::showBatteries(const HomeInfoData& home, const uint64_t homeAt){
	char text[24];
	if(homeAt == 0){
		setText(botPct, "Bot ...");
		setBar(botBar, 0);
	}else{
		switch(robotState->getChargeState()){
			case ChargeStatus::Charging: snprintf(text, sizeof(text), "Bot %u%% chg", home.botPercent); break;
			case ChargeStatus::Full: snprintf(text, sizeof(text), "Bot full"); break;
			default: snprintf(text, sizeof(text), "Bot %u%%", home.botPercent); break;
		}
		setText(botPct, text);
		setBar(botBar, robotState->getChargeState() == ChargeStatus::Full ? 100 : home.botPercent);
	}

	const uint8_t own = battery ? battery->getPerc() : 0;
	snprintf(text, sizeof(text), "Remote %u%%", own);
	setText(ctrlPct, text);
	setBar(ctrlBar, own);
}

void HomeDash::showAi(const HomeInfoData& home, const uint64_t homeAt){
	const bool ai = robotState->isAi();
	lv_obj_update_flag(hints, LV_OBJ_FLAG_HIDDEN, ai);
	for(lv_obj_t* l : { next, seen, seenAge, said }){
		lv_obj_update_flag(l, LV_OBJ_FLAG_HIDDEN, !ai);
	}
	if(!ai){
		setText(mood, "");
		setText(mode, "AI mode off");
		setText(activity, "");
		lv_obj_set_style_bg_opa(activity, LV_OPA_TRANSP, 0);
		return;
	}

	// His mood, faint when it is faint
	const uint8_t emotion = robotState->getAiEmotion();
	const uint8_t strength = robotState->getAiIntensity();
	char text[48];
	snprintf(text, sizeof(text), "%s%s", strength >= 70 ? "very " : "", emotion < std::size(Emotions) ? Emotions[emotion] : "");
	setText(mood, text);

	using M = AIStatusData::Mode;
	switch(robotState->getAiMode()){
		case M::Listening: snprintf(text, sizeof(text), "listening"); break;
		case M::Attentive: snprintf(text, sizeof(text), "chatting"); break;
		case M::Thinking: snprintf(text, sizeof(text), "thinking..."); break;
		case M::NoBrain: snprintf(text, sizeof(text), "no agent"); break;
		case M::Focus: {
			const uint8_t m = robotState->getAiFocusMinutes();
			if(m == 0 || m == 255){
				snprintf(text, sizeof(text), "focus");
			}else{
				snprintf(text, sizeof(text), "focus %um", m);
			}
			break;
		}
		default: text[0] = 0; break;
	}
	setText(mode, text);

	// The activity level, shown inverted for a moment after the joystick changed it
	const bool flash = millis() < activityShownUntil;
	const auto level = (uint8_t) (flash ? activityShown : robotState->getAiActivity());
	snprintf(text, sizeof(text), "pace: %s", level < std::size(Activities) ? Activities[level] : "");
	setText(activity, text);
	lv_obj_set_style_bg_opa(activity, flash ? LV_OPA_COVER : LV_OPA_TRANSP, 0);
	lv_obj_set_style_text_color(activity, flash ? theme->getTertiaryColor() : theme->getSecondaryColor(), 0);

	const std::string nextText = home.next.empty() ? "No reminders" : "Next " + home.next;
	setText(next, nextText.c_str());

	// A new sighting: he looks surprised for a moment.
	if(!home.seen.empty() && home.seen != lastSeen && !lastSeen.empty() && home.seenSecs < 30){
		face->startled();
	}
	lastSeen = home.seen;
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

	face = new BotFace(*this);
	lv_obj_set_pos(*face, 0, 0);

	mood = addLabel(ColX, MoodY, ColW, LV_TEXT_ALIGN_LEFT, prim);
	mode = addLabel(ColX, ModeY, ColW, LV_TEXT_ALIGN_LEFT, second);
	activity = addLabel(ColX, ActivityY, ColW, LV_TEXT_ALIGN_LEFT, second);
	lv_obj_set_style_bg_color(activity, prim, 0);
	botPct = addLabel(ColX, BotY, ColW, LV_TEXT_ALIGN_LEFT, prim);
	botBar = addBar(ColX, BotY + BarGap, ColW);
	ctrlPct = addLabel(ColX, CtrlY, ColW, LV_TEXT_ALIGN_LEFT, prim);
	ctrlBar = addBar(ColX, CtrlY + BarGap, ColW);

	next = addLabel(0, NextY, Width, LV_TEXT_ALIGN_LEFT, prim);
	lv_label_set_long_mode(next, LV_LABEL_LONG_DOT);
	seen = addLabel(0, SeenY, 100, LV_TEXT_ALIGN_LEFT, prim);
	lv_label_set_long_mode(seen, LV_LABEL_LONG_DOT);
	seenAge = addLabel(100, SeenY, 20, LV_TEXT_ALIGN_RIGHT, second);
	said = addLabel(0, SaidY, Width, LV_TEXT_ALIGN_LEFT, second);
	lv_label_set_long_mode(said, LV_LABEL_LONG_SCROLL_CIRCULAR);
	playing = addLabel(0, PlayingY, Width, LV_TEXT_ALIGN_LEFT, second);
	lv_label_set_long_mode(playing, LV_LABEL_LONG_DOT);

	hints = addLabel(0, NextY, Width, LV_TEXT_ALIGN_LEFT, second);
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
	lv_label_set_text(l, "");
	return l;
}

lv_obj_t* HomeDash::addBar(const int32_t x, const int32_t y, const int32_t w){
	lv_obj_t* b = lv_bar_create(*this);
	lv_obj_set_size(b, w, 2);
	lv_obj_set_pos(b, x, y);
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

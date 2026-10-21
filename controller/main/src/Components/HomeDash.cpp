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
	constexpr int32_t BarGap = 8; // from the battery line to its bar
	constexpr int32_t ExtraY = 47;
	constexpr int32_t NextY = 63;
	constexpr int32_t SeenY = 73;
	constexpr int32_t BubbleY = 83;
	constexpr int32_t BubbleH = 29;
	// The controller runs on AA batteries or USB; only low batteries are worth a word.
	constexpr uint8_t ControllerLow = 25;
	// A bubble stays bright this long after it changed
	constexpr uint64_t BubbleFreshMs = 20000;

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
}

void HomeDash::showBatteries(const HomeInfoData& home, const uint64_t homeAt){
	char text[24];
	if(homeAt == 0){
		setText(botPct, "Bot ...");
		setBar(botBar, 0);
	}else{
		// Plugged in shows as a bolt in the top bar
		if(robotState->getChargeState() == ChargeStatus::Full){
			snprintf(text, sizeof(text), "Bot full");
		}else{
			snprintf(text, sizeof(text), "Bot %u%%", home.botPercent);
		}
		setText(botPct, text);
		setBar(botBar, robotState->getChargeState() == ChargeStatus::Full ? 100 : home.botPercent);
	}

	const uint8_t own = battery ? battery->getPerc() : 100;
	if(own <= ControllerLow){
		snprintf(text, sizeof(text), "Remote AAs %u%%", own);
		setText(extra, text);
		lv_obj_set_style_text_color(extra, theme->getPrimaryColor(), 0);
	}else{
		const std::string music = home.playing.empty() ? "" : "Playing: " + home.playing;
		setText(extra, music.c_str());
		lv_obj_set_style_text_color(extra, theme->getSecondaryColor(), 0);
	}
}

void HomeDash::showAi(const HomeInfoData& home, const uint64_t homeAt){
	const bool ai = robotState->isAi();
	lv_obj_update_flag(hints, LV_OBJ_FLAG_HIDDEN, ai);
	for(lv_obj_t* l : { next, seen, seenAge, bubble }){
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
	if(!home.doing.empty() && robotState->getAiMode() != M::Focus && robotState->getAiMode() != M::NoBrain){
		snprintf(text, sizeof(text), "%s", home.doing.c_str());
	}else switch(robotState->getAiMode()){
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

	// A reminder, or his own next plan ("Plan ...")
	const std::string nextText = home.next.empty() || home.next.rfind("Plan", 0) == 0 ? home.next : "Next " + home.next;
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

	showBubble(home);
}

// What he says, as he says it, or what was just said to him. Bright while he talks and for a
// little while after; then it fades to the secondary colour until something new is said.
void HomeDash::showBubble(const HomeInfoData& home){
	std::string text;
	if(home.heardLast && !home.heard.empty()){
		text = "You: " + home.heard;
	}else{
		text = home.said;
	}
	lv_obj_update_flag(bubble, LV_OBJ_FLAG_HIDDEN, text.empty());
	if(text != bubbleShown){
		bubbleShown = text;
		bubbleChangedAt = millis();
		setText(bubbleText, text.c_str());
	}
	const bool bright = robotState->isAiSpeaking() || millis() - bubbleChangedAt < BubbleFreshMs;
	const lv_color_t c = bright ? theme->getPrimaryColor() : theme->getSecondaryColor();
	lv_obj_set_style_border_color(bubble, c, 0);
	lv_obj_set_style_text_color(bubbleText, c, 0);
	lv_obj_set_style_border_width(bubble, robotState->isAiSpeaking() ? 2 : 1, 0);
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
	extra = addLabel(ColX, ExtraY, ColW, LV_TEXT_ALIGN_LEFT, second);
	lv_label_set_long_mode(extra, LV_LABEL_LONG_SCROLL_CIRCULAR);

	next = addLabel(0, NextY, Width, LV_TEXT_ALIGN_LEFT, prim);
	lv_label_set_long_mode(next, LV_LABEL_LONG_DOT);
	seen = addLabel(0, SeenY, 100, LV_TEXT_ALIGN_LEFT, prim);
	lv_label_set_long_mode(seen, LV_LABEL_LONG_DOT);
	seenAge = addLabel(100, SeenY, 20, LV_TEXT_ALIGN_RIGHT, second);

	bubble = lv_obj_create(*this);
	lv_obj_remove_flag(bubble, LV_OBJ_FLAG_SCROLLABLE);
	lv_obj_set_size(bubble, Width, BubbleH);
	lv_obj_set_pos(bubble, 0, BubbleY);
	lv_obj_set_style_bg_opa(bubble, LV_OPA_TRANSP, 0);
	lv_obj_set_style_border_color(bubble, second, 0);
	lv_obj_set_style_border_width(bubble, 1, 0);
	lv_obj_set_style_radius(bubble, 4, 0);
	lv_obj_set_style_pad_all(bubble, 2, 0);
	bubbleText = lv_label_create(bubble);
	lv_obj_set_style_text_font(bubbleText, &lv_font_butter, 0);
	lv_obj_set_style_text_color(bubbleText, second, 0);
	lv_obj_set_style_text_line_space(bubbleText, 1, 0);
	lv_label_set_long_mode(bubbleText, LV_LABEL_LONG_SCROLL);
	lv_obj_set_size(bubbleText, Width - 6, BubbleH - 6);
	lv_label_set_text(bubbleText, "");

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

#include "BotFace.h"

#include <algorithm>
#include <cstdlib>
#include <Util/stdafx.h>

namespace {
	// The plane's emotions (AIStatusData::emotion).
	enum Emotion : uint8_t {
		Content, Happy, Excited, Curious, Playful, Calm, Sleepy, Bored, Sad, Grumpy, Surprised, Worried
	};

	constexpr int32_t LeftEyeX = 9, RightEyeX = 20, EyeY = 11, MouthY = 22;

	uint64_t randomIn(uint64_t lo, uint64_t hi){
		return lo + (uint64_t) rand() % (hi - lo + 1);
	}
}

BotFace::BotFace(lv_obj_t* parent) : LVObject(parent){
	theme = Application::getApp()->getService<ThemeService>();
	robotState = Application::getApp()->getService<RobotState>();
	ink = lv_color_to_u16(theme->getPrimaryColor());
	paper = lv_color_to_u16(theme->getTertiaryColor());

	lv_obj_set_size(*this, Size * Scale, Size * Scale);
	lv_obj_remove_flag(*this, LV_OBJ_FLAG_SCROLLABLE);
	lv_obj_set_style_bg_opa(*this, LV_OPA_TRANSP, 0);

	canvas = lv_canvas_create(*this);
	lv_canvas_set_buffer(canvas, buf, Size * Scale, Size * Scale, LV_COLOR_FORMAT_RGB565);
	lv_obj_set_pos(canvas, 0, 0);
}

void BotFace::poked(){
	pokedUntil = millis() + 700;
}

void BotFace::startled(){
	startledUntil = millis() + 1200;
}

void BotFace::loop(){
	const Look l = compose(millis());
	if(drawn && l == shown){
		return;
	}
	draw(l);
	shown = l;
	drawn = true;
}

BotFace::Look BotFace::compose(const uint64_t now){
	using M = AIStatusData::Mode;
	const M mode = robotState->getAiMode();
	const uint8_t emotion = robotState->getAiEmotion();
	const bool strong = robotState->getAiIntensity() >= 60;

	Look l;
	// How much he looks about, and how often he blinks.
	uint64_t glanceMin = 2500, glanceMax = 7000;
	uint64_t blinkMin = 2500, blinkMax = 6000, blinkMs = 130;

	switch(emotion){
		case Happy:
			l.left = l.right = Eye::Happy;
			l.mouth = Mouth::Smile;
			l.mouthW = 10;
			l.mouthH = strong ? 3 : 2;
			break;
		case Excited:
			l.eyeW = 6;
			l.eyeH = 9;
			l.mouth = Mouth::Grin;
			l.mouthW = 10;
			l.mouthH = 3;
			l.bob = (now / 250) % 2;
			glanceMin = 800;
			glanceMax = 2000;
			blinkMin = 1500;
			blinkMax = 4000;
			break;
		case Curious:
			l.rightEyeH = 9;
			l.brows = Brows::Raised;
			l.mouth = Mouth::O;
			l.mouthW = 4;
			l.mouthH = 4;
			glanceMin = 1200;
			glanceMax = 3000;
			break;
		case Playful:
			l.right = Eye::Happy;
			l.mouth = Mouth::Smirk;
			l.mouthW = 9;
			break;
		case Calm:
			l.left = l.right = Eye::Lidded;
			l.eyeH = 4;
			l.mouthW = 6;
			l.mouthH = 1;
			glanceMin = 5000;
			glanceMax = 10000;
			break;
		case Sleepy:
			l.left = l.right = Eye::Lidded;
			l.eyeH = 3;
			l.mouth = Mouth::Flat;
			l.mouthW = 4;
			l.zzz = strong;
			glanceMin = 8000;
			glanceMax = 15000;
			blinkMin = 3000;
			blinkMax = 7000;
			blinkMs = 600;
			break;
		case Bored:
			l.left = l.right = Eye::Lidded;
			l.eyeH = 4;
			l.mouth = Mouth::Flat;
			l.mouthW = 6;
			glanceMin = 6000;
			glanceMax = 12000;
			break;
		case Sad:
			l.brows = Brows::Sad;
			l.eyeH = 6;
			l.dy = 1;
			l.mouth = Mouth::Frown;
			l.mouthW = 8;
			l.mouthH = strong ? 2 : 1;
			glanceMin = 6000;
			glanceMax = 12000;
			break;
		case Grumpy:
			l.brows = Brows::Angry;
			l.left = l.right = Eye::Lidded;
			l.eyeH = 4;
			l.mouth = strong ? Mouth::Frown : Mouth::Flat;
			l.mouthW = 8;
			l.mouthH = 1;
			break;
		case Surprised:
			l.eyeW = 6;
			l.eyeH = 9;
			l.brows = Brows::Raised;
			l.mouth = Mouth::O;
			l.mouthW = 5;
			l.mouthH = 5;
			break;
		case Worried:
			l.brows = Brows::Sad;
			l.mouth = Mouth::Wavy;
			l.mouthW = 8;
			glanceMin = 700;
			glanceMax = 1800;
			break;
		case Content:
		default:
			l.mouthH = 1;
			break;
	}

	// What he is doing goes on top of how he feels.
	bool glances = true;
	switch(mode){
		case M::NoBrain:
			l = Look{};
			l.left = l.right = Eye::Closed;
			l.mouth = Mouth::Flat;
			l.mouthW = 4;
			l.zzz = true;
			glances = false;
			break;
		case M::Focus:
			if(l.left == Eye::Open || l.left == Eye::Happy){
				l.left = l.right = Eye::Lidded;
				l.eyeH = 4;
			}
			l.dy = 1;
			l.brows = Brows::None;
			l.mouth = Mouth::Flat;
			l.mouthW = 5;
			l.bob = 0;
			glances = false;
			break;
		case M::Thinking:
			l.dx = 2;
			l.dy = -2;
			l.mouth = Mouth::Smirk;
			l.mouthW = 6;
			glances = false;
			break;
		case M::Attentive:
			if(l.left == Eye::Open){
				l.eyeH = std::min<int8_t>(l.eyeH + 1, 9);
			}
			glanceMin *= 2; // mostly looking at you
			glanceMax *= 2;
			break;
		default:
			break;
	}

	if(glances){
		if(now >= nextGlance){
			static constexpr int8_t Xs[] = { -2, 0, 0, 2 };
			static constexpr int8_t Ys[] = { -1, 0, 0, 1 };
			glanceX = Xs[rand() % 4];
			glanceY = Ys[rand() % 4];
			nextGlance = now + randomIn(glanceMin, glanceMax);
		}
		l.dx += emotion == Bored ? -2 : glanceX;
		l.dy += glanceY;
	}

	if(robotState->isAiSpeaking() && mode != M::NoBrain){
		if(now >= nextTalk){
			talkH = (int8_t) randomIn(1, 4);
			nextTalk = now + randomIn(90, 180);
		}
		l.mouth = Mouth::Talk;
		l.mouthW = std::max<int8_t>(l.mouthW, 6);
		l.mouthH = talkH;
	}

	if(l.left != Eye::Closed){
		if(now >= nextBlink){
			blinkUntil = now + blinkMs;
			nextBlink = now + randomIn(blinkMin, blinkMax);
		}
		if(now < blinkUntil){
			l.left = l.right = Eye::Closed;
		}
	}

	if(now < startledUntil){
		l.left = l.right = Eye::Open;
		l.eyeW = 6;
		l.eyeH = 9;
		l.rightEyeH = 0;
		l.brows = Brows::Raised;
		l.mouth = Mouth::O;
		l.mouthW = 5;
		l.mouthH = 5;
	}
	if(now < pokedUntil){
		l.left = l.right = Eye::Squint;
		l.brows = Brows::None;
		l.mouth = Mouth::O;
		l.mouthW = 4;
		l.mouthH = 4;
		l.dx = l.dy = 0;
	}
	if(l.zzz){
		l.zzz = (now / 700) % 3 != 0;
	}
	return l;
}

void BotFace::draw(const Look& l){
	std::fill(std::begin(buf), std::end(buf), paper);

	const int32_t y0 = EyeY + l.dy - l.bob;
	const int32_t rightH = l.rightEyeH ? l.rightEyeH : l.eyeH;
	drawEye(l.left, LeftEyeX + l.dx, y0, l.eyeW, l.eyeH, true);
	drawEye(l.right, RightEyeX + l.dx, y0, l.eyeW, rightH, false);

	const int32_t browY = y0 - std::max<int32_t>(l.eyeH, rightH) / 2 - 3;
	for(const int32_t cx : { LeftEyeX + (int32_t) l.dx, RightEyeX + (int32_t) l.dx }){
		const bool left = cx < 15;
		switch(l.brows){
			case Brows::Raised:
				px(cx - 2, browY + 1);
				line(cx - 1, browY, cx + 2, browY);
				break;
			case Brows::Angry: // inner ends low
				if(left) line(cx - 3, browY, cx + 2, browY + 2);
				else line(cx - 2, browY + 2, cx + 3, browY);
				break;
			case Brows::Sad: // inner ends high
				if(left) line(cx - 3, browY + 2, cx + 2, browY);
				else line(cx - 2, browY, cx + 3, browY + 2);
				break;
			case Brows::None:
				break;
		}
	}

	drawMouth(l.mouth, 15 + l.dx / 2, MouthY + l.dy / 2 - l.bob, l.mouthW, l.mouthH);

	if(l.zzz){
		// A little "z" up on the right
		line(24, 1, 27, 1);
		line(26, 2, 25, 3);
		line(24, 4, 27, 4);
	}

	lv_obj_invalidate(canvas);
}

void BotFace::px(const int32_t x, const int32_t y, const bool on){
	if(x < 0 || y < 0 || x >= Size || y >= Size){
		return;
	}
	for(int32_t j = 0; j < Scale; j++){
		for(int32_t i = 0; i < Scale; i++){
			buf[(y * Scale + j) * Size * Scale + x * Scale + i] = on ? ink : paper;
		}
	}
}

void BotFace::rect(const int32_t x, const int32_t y, const int32_t w, const int32_t h){
	for(int32_t j = 0; j < h; j++){
		for(int32_t i = 0; i < w; i++){
			px(x + i, y + j);
		}
	}
}

void BotFace::line(int32_t x0, int32_t y0, const int32_t x1, const int32_t y1){
	const int32_t dx = std::abs(x1 - x0), sx = x0 < x1 ? 1 : -1;
	const int32_t dy = -std::abs(y1 - y0), sy = y0 < y1 ? 1 : -1;
	int32_t err = dx + dy;
	while(true){
		px(x0, y0);
		if(x0 == x1 && y0 == y1) break;
		const int32_t e2 = 2 * err;
		if(e2 >= dy){ err += dy; x0 += sx; }
		if(e2 <= dx){ err += dx; y0 += sy; }
	}
}

void BotFace::drawEye(const Eye e, const int32_t cx, const int32_t cy, const int32_t w, const int32_t h, const bool left){
	const int32_t x = cx - w / 2;
	switch(e){
		case Eye::Open: {
			const int32_t y = cy - h / 2;
			rect(x, y, w, h);
			if(w >= 4 && h >= 4){ // rounded
				px(x, y, false);
				px(x + w - 1, y, false);
				px(x, y + h - 1, false);
				px(x + w - 1, y + h - 1, false);
			}
			break;
		}
		case Eye::Lidded: {
			// The bottom of an open eye, under a flat lid
			const int32_t bottom = cy + 3;
			rect(x, bottom - h + 1, w, h);
			px(x, bottom, false);
			px(x + w - 1, bottom, false);
			break;
		}
		case Eye::Closed:
			// A shallow "u"
			px(x, cy + 1);
			line(x + 1, cy + 2, x + w - 2, cy + 2);
			px(x + w - 1, cy + 1);
			break;
		case Eye::Happy:
			// "^"
			for(int32_t i = 0; i < w; i++){
				const int32_t d = std::min(i, w - 1 - i);
				px(x + i, cy + 1 - d);
				px(x + i, cy + 2 - d);
			}
			break;
		case Eye::Squint:
			// ">" and "<"
			if(left){
				line(cx - 2, cy - 2, cx + 1, cy);
				line(cx + 1, cy, cx - 2, cy + 2);
			}else{
				line(cx + 2, cy - 2, cx - 1, cy);
				line(cx - 1, cy, cx + 2, cy + 2);
			}
			break;
	}
}

void BotFace::drawMouth(const Mouth m, const int32_t cx, const int32_t cy, const int32_t w, const int32_t h){
	const int32_t x = cx - w / 2;
	switch(m){
		case Mouth::Flat:
			line(x, cy, x + w - 1, cy);
			break;
		case Mouth::Smile:
			for(int32_t i = 0; i < w; i++){
				px(x + i, cy + std::min(std::min(i, w - 1 - i), h));
			}
			break;
		case Mouth::Grin:
			for(int32_t i = 0; i < w; i++){
				const int32_t depth = std::min(std::min(i, w - 1 - i), h);
				line(x + i, cy, x + i, cy + depth);
			}
			break;
		case Mouth::Frown:
			for(int32_t i = 0; i < w; i++){
				px(x + i, cy + h - std::min(std::min(i, w - 1 - i), h));
			}
			break;
		case Mouth::O: {
			const int32_t y = cy - h / 2;
			line(x + 1, y, x + w - 2, y);
			line(x + 1, y + h - 1, x + w - 2, y + h - 1);
			line(x, y + 1, x, y + h - 2);
			line(x + w - 1, y + 1, x + w - 1, y + h - 2);
			break;
		}
		case Mouth::Talk:
			rect(x + 1, cy - h / 2, w - 2, h);
			if(h >= 3){
				line(x, cy - h / 2 + 1, x, cy - h / 2 + h - 2);
				line(x + w - 1, cy - h / 2 + 1, x + w - 1, cy - h / 2 + h - 2);
			}
			break;
		case Mouth::Wavy:
			for(int32_t i = 0; i < w; i++){
				px(x + i, cy + ((i / 2) % 2));
			}
			break;
		case Mouth::Smirk:
			for(int32_t i = 0; i < w; i++){
				px(x + i, i < w / 2 ? cy + 1 : cy + 1 - (i - w / 2 + 1) / 2);
			}
			break;
	}
}

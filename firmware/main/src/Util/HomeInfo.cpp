#include "HomeInfo.h"
#include <algorithm>
#include <cstdlib>
#include <mutex>
#include <Core/Application.h>
#include <Util/stdafx.h>
#include "Battery/Battery.h"
#include "Phone/Phone.h"
#include "Services/Com.h"
#include "Services/Time.h"

namespace {
	std::mutex lock;
	HomeInfoData news;
	uint64_t newsAt = 0; // when the plane sent it (seenSecs counts from then)

	/** `s` without bytes outside ASCII: the controller's font has nothing else. */
	std::string ascii(const std::string& s){
		std::string out;
		out.reserve(s.size());
		for(const char c : s){
			if((unsigned char) c >= 0x20 && (unsigned char) c < 0x7f){
				out += c;
			}
		}
		return out;
	}

	/** Local time fields as a time_t (the robot keeps local time; the C library has no zone set). */
	time_t toTime(tm t){
		return mktime(&t);
	}
}

void HomeInfo::setAi(const HomeInfoData& n){
	std::lock_guard guard(lock);
	news.next = n.next;
	news.seen = n.seen;
	news.seenSecs = n.seenSecs;
	news.said = n.said;
	news.heard = n.heard;
	news.heardLast = n.heardLast;
	news.doing = n.doing;
	newsAt = millis();
}

void HomeInfo::clearAi(){
	std::lock_guard guard(lock);
	news = {};
	newsAt = 0;
}

void HomeInfo::syncClock(const HomeInfoData& plane){
	Time* time = Application::getApp()->getService<Time>();
	if(time == nullptr || !plane.clockSet || plane.year < 2024){
		return;
	}
	tm t{};
	t.tm_year = plane.year - 1900;
	t.tm_mon = plane.month - 1;
	t.tm_mday = plane.day;
	t.tm_hour = plane.hour;
	t.tm_min = plane.minute;
	t.tm_sec = plane.second;
	const time_t want = toTime(t);
	if(time->isConfigured() && std::llabs((long long) (toTime(time->getTime()) - want)) <= 5){
		return;
	}
	time->setTime(want);
}

void HomeInfo::send(){
	const Application* app = Application::getApp();
	Com* com = app->getService<Com>();
	if(com == nullptr || com->getStatus() != Com::ConnStatus::Connected){
		return;
	}

	HomeInfoData d;
	{
		std::lock_guard guard(lock);
		d = news;
		if(newsAt != 0 && !d.seen.empty()){
			const uint64_t since = (millis() - newsAt) / 1000;
			d.seenSecs = (uint16_t) std::min<uint64_t>(d.seenSecs + since, UINT16_MAX);
		}
	}

	if(const Battery* battery = app->getService<Battery>()){
		d.botPercent = (uint8_t) std::clamp(battery->getPerc(), 0.0f, 100.0f);
	}

	if(Phone* phone = app->getService<Phone>(); phone != nullptr && phone->getMediaState() == MediaState::Playing){
		const MediaInfo media = phone->getMedia();
		d.playing = ascii(media.artist.empty() ? media.title : media.title + " - " + media.artist);
	}

	if(Time* time = app->getService<Time>(); time != nullptr && time->isConfigured()){
		tm t = time->getTime();
		mktime(&t); // fills in the weekday
		d.clockSet = true;
		d.year = t.tm_year + 1900;
		d.month = t.tm_mon + 1;
		d.day = t.tm_mday;
		d.weekday = t.tm_wday;
		d.hour = t.tm_hour;
		d.minute = t.tm_min;
		d.second = t.tm_sec;
	}

	com->sendData(BB::State::Idle, BB::Action::Idle::Home, d);
}

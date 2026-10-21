#ifndef BUTTERBOTCTRL_FIRMWARE_ROBOTSTATE_H
#define BUTTERBOTCTRL_FIRMWARE_ROBOTSTATE_H

#include <atomic>
#include <cstdint>
#include <ctime>
#include <mutex>
#include <Core/Application.h>
#include <Util/stdafx.h>
#include <BBData.h>

/**
 * @brief Holds state of the currently connected ButterBot, shared across the application.
 */
class RobotState : public Object {
	GENERATED_BODY(RobotState, Object, void)

public:
	uint8_t getBotBatteryLevel() const{ return botBatteryLevel.load(); }
	void setBotBatteryLevel(uint8_t level){ botBatteryLevel.store(level); }

	ChargeStatus getChargeState() const{ return chargeState.load(); }
	void setChargeState(ChargeStatus state){ chargeState.store(state); }

	bool isIdleNone() const{ return idleNone.load(); }
	void setIdleNone(bool value){ idleNone.store(value); }

	bool isMuted() const{ return muted.load(); }
	void setMuted(bool value){ muted.store(value); }

	/** AI mode (the laptop control plane), as the robot last reported it. */
	bool isAi() const{ return aiMode.load() != (uint8_t) AIStatusData::Mode::Off; }
	AIStatusData::Mode getAiMode() const{ return (AIStatusData::Mode) aiMode.load(); }
	AIStatusData::Activity getAiActivity() const{ return (AIStatusData::Activity) aiActivity.load(); }
	uint8_t getAiFocusMinutes() const{ return aiFocus.load(); }
	bool isAiSpeaking() const{ return aiSpeaking.load(); }
	/** Its mood: the plane's emotion index (see AIStatusData) and strength 0-100. */
	uint8_t getAiEmotion() const{ return aiEmotion.load(); }
	uint8_t getAiIntensity() const{ return aiIntensity.load(); }
	void setAi(const AIStatusData& s){
		aiMode.store((uint8_t) s.mode);
		aiActivity.store((uint8_t) s.activity);
		aiFocus.store(s.focusMinutes);
		aiSpeaking.store(s.speaking);
		aiEmotion.store(s.emotion);
		aiIntensity.store(s.intensity);
	}

	/** The home screen's info (battery, clock, AI mode's news), as the robot last sent it. */
	HomeInfoData getHome() const{
		std::lock_guard guard(homeLock);
		return home;
	}
	/** When the home info arrived (millis): the clock has run on since. */
	uint64_t getHomeAt() const{ return homeAt.load(); }
	/** Changes with every update. */
	uint32_t getHomeVersion() const{ return homeVersion.load(); }
	void setHome(HomeInfoData h){
		{
			std::lock_guard guard(homeLock);
			home = std::move(h);
		}
		homeAt.store(millis());
		homeVersion.fetch_add(1);
	}
	/** The robot's clock, run on since it was sent; false while it has none. */
	bool getClock(tm& out) const{
		const uint64_t at = homeAt.load();
		HomeInfoData h;
		{
			std::lock_guard guard(homeLock);
			if(at == 0 || !home.clockSet){
				return false;
			}
			h.year = home.year;
			h.month = home.month;
			h.day = home.day;
			h.hour = home.hour;
			h.minute = home.minute;
			h.second = home.second;
		}
		tm t{};
		t.tm_year = h.year - 1900;
		t.tm_mon = h.month - 1;
		t.tm_mday = h.day;
		t.tm_hour = h.hour;
		t.tm_min = h.minute;
		t.tm_sec = h.second;
		// No time zone is set here, so the round trip only does the calendar arithmetic.
		const time_t now = mktime(&t) + (time_t) ((millis() - at) / 1000);
		localtime_r(&now, &out);
		return true;
	}

	/** The robot went away: its news is stale (the clock keeps running). */
	void clearHomeNews(){
		{
			std::lock_guard guard(homeLock);
			home.next.clear();
			home.seen.clear();
			home.said.clear();
		}
		homeVersion.fetch_add(1);
	}

private:
	std::atomic<uint8_t> botBatteryLevel{ 0 };
	std::atomic<ChargeStatus> chargeState{ ChargeStatus::Unplugged };
	std::atomic<bool> idleNone{ true };
	std::atomic<bool> muted{ false };
	std::atomic<uint8_t> aiMode{ 0 };
	std::atomic<uint8_t> aiActivity{ (uint8_t) AIStatusData::Activity::Normal };
	std::atomic<uint8_t> aiFocus{ 0 };
	std::atomic<bool> aiSpeaking{ false };
	std::atomic<uint8_t> aiEmotion{ 0 };
	std::atomic<uint8_t> aiIntensity{ 0 };
	mutable std::mutex homeLock;
	HomeInfoData home;
	std::atomic<uint64_t> homeAt{ 0 };
	std::atomic<uint32_t> homeVersion{ 0 };
};

#endif //BUTTERBOTCTRL_FIRMWARE_ROBOTSTATE_H

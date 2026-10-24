#ifndef BUTTERBOT_FIRMWARE_LINKPROTO_H
#define BUTTERBOT_FIRMWARE_LINKPROTO_H

#include <cstdint>
#include <cstddef>

/**
 * Wire protocol of the AI control plane link. Mirrors plane/crates/bbproto on the laptop.
 *
 * UDP, like the stock RC video feed: lost packets are skipped, never waited for. Every datagram
 * carries exactly one frame:
 *   magic u16 LE (0xBB01) | type u8 | flags u8 | len u32 LE | payload[len]
 * The robot announces itself with "hello" until the plane answers (the plane pings every second);
 * requests with an id are retransmitted by the plane until acked, and de-duplicated here.
 */
namespace LinkProto {

inline constexpr uint16_t Magic = 0xBB01;
inline constexpr size_t HeaderLen = 8;
inline constexpr size_t MaxPayload = 1024 * 1024;
inline constexpr uint32_t Version = 2;
// Largest frame payload in one datagram (stock feed uses <= 1440-byte datagrams).
inline constexpr size_t MaxDatagramPayload = 1400;
inline constexpr size_t MicSamplesPerDatagram = 512; // 32 ms
inline constexpr const char* FirmwareName = "butterbot-ai 0.4";

enum class FrameType : uint8_t {
	Ctrl = 1,   // JSON, both directions
	Video = 2,  // robot -> plane: seq u32, ts_ms u32, w u16, h u16, frag u16, frags u16, JPEG part
	MicPcm = 3, // robot -> plane: seq u32, ts_ms u32, PCM s16le 16 kHz mono
	SpkPcm = 4, // plane -> robot: PCM s16le 16 kHz mono
};

// Video frame flag: answers a snapshot request; seq carries the request id.
inline constexpr uint8_t FlagSnapshot = 0x01;

inline constexpr size_t MediaHeaderLen = 8;
inline constexpr size_t VideoHeaderLen = MediaHeaderLen + 8;

// Ctrl::LinkConfig confirmation sent back over BLE: BB state Idle, this action value, payload u8 ok.
inline constexpr uint32_t LinkConfigAckAction = 100;
// Ctrl::LinkWake confirmation: payload u8 ok (0 = no stored configuration).
inline constexpr uint32_t LinkWakeAckAction = 101;
// Ctrl::LinkTune confirmation: payload u8 ok.
inline constexpr uint32_t LinkTuneAckAction = 102;

inline void putU16(uint8_t* p, uint16_t v){ p[0] = v; p[1] = v >> 8; }
inline void putU32(uint8_t* p, uint32_t v){ p[0] = v; p[1] = v >> 8; p[2] = v >> 16; p[3] = v >> 24; }
inline uint16_t getU16(const uint8_t* p){ return p[0] | (p[1] << 8); }
inline uint32_t getU32(const uint8_t* p){ return p[0] | (p[1] << 8) | (p[2] << 16) | ((uint32_t) p[3] << 24); }

inline void putHeader(uint8_t* p, FrameType type, uint8_t flags, uint32_t len){
	putU16(p, Magic);
	p[2] = (uint8_t) type;
	p[3] = flags;
	putU32(p + 4, len);
}

}

#endif //BUTTERBOT_FIRMWARE_LINKPROTO_H

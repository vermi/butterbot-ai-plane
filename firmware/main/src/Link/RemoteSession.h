#ifndef BUTTERBOT_FIRMWARE_REMOTESESSION_H
#define BUTTERBOT_FIRMWARE_REMOTESESSION_H

#include <atomic>
#include <memory>
#include <mutex>
#include <esp_camera.h>
#include "Thread/Threaded.h"
#include "Audio/PCMStream.h"

class LinkService;
class Camera;
class Audio;

/**
 * Media side of remote (AI) mode: camera and microphone streaming threads and the speaker
 * stream. Lives on the heap, owned by RemoteState, so the state stays within its fixed buffer.
 */
class RemoteSession {
public:
	RemoteSession(LinkService* link, Camera* camera, Audio* audio);
	~RemoteSession();

	void setCamera(bool on, framesize_t res, uint8_t fps, uint8_t quality, bool swJpeg, uint8_t xclkMhz, int8_t psramDma);
	void requestSnapshot(uint32_t id, framesize_t res, uint8_t quality);
	void setMic(bool on, bool gate, float gain);

	void speakerBegin();
	void speakerEnd();
	void speakerAbort();
	bool speakerStreaming() const;
	uint32_t speakerUnderruns() const { return stream->underruns(); }

	/**
	 * Stop touching the camera and microphone so a stock routine can use them
	 * (they expect 128x128 RGB565 and own the mic through AudioFrontend). Blocks until idle.
	 */
	bool pause(bool paused);

private:
	LinkService* link;
	Camera* camera;
	Audio* audio;

	struct CamConfig {
		bool on = false;
		framesize_t res = FRAMESIZE_128X128; // the size the stock RC feed streams reliably
		uint8_t fps = 15;
		uint8_t quality = 12;
		bool swJpeg = true;   // also used by snapshots
		uint8_t xclkMhz = 0;
		int8_t psramDma = -1;
	};
	std::mutex camMutex;
	CamConfig camConfig;
	bool snapshotPending = false;
	uint32_t snapshotId = 0;
	framesize_t snapshotRes = FRAMESIZE_VGA;
	uint8_t snapshotQuality = 10;

	std::atomic_bool paused = false;
	// Held by the camera/mic threads while they touch the hardware; pause() takes both, so a
	// stock routine never shares the camera driver or the mic with them.
	std::mutex camHw;
	std::mutex micHw;
	int lastQuality = -1;

	std::atomic_bool micOn = false;
	std::atomic_bool micGate = true;
	std::atomic<float> micGain = 1.0f;

	uint32_t frameSeq = 0;
	uint32_t micSeq = 0;
	uint64_t nextFrameAt = 0;
	bool streamFormatActive = false;

	std::shared_ptr<PCMStream> stream;
	PassthroughGen passthrough;
	std::atomic_bool speakerActive = false;

	static constexpr size_t MicChunk = 2048; // 128 ms; matches MicInput's internal read size
	int16_t* micBuf = nullptr;

	std::unique_ptr<Threaded> camThread;
	std::unique_ptr<Threaded> micThread;
	void camLoop();
	void micLoop();

	bool cameraFits(framesize_t res, bool jpeg = true, bool psramDma = true);
	bool applyCapture(framesize_t res, uint8_t quality, const CamConfig& opts);
	/** Grab one frame and hand it to the link as JPEG (encoding RGB565 frames on the CPU). */
	bool captureAndSend(uint8_t quality, bool swJpeg, uint32_t seq, bool snapshot, uint32_t ackId = 0);
	bool appliedSw = false;
	uint8_t appliedXclk = 0;
	int8_t appliedDma = -1;
	bool restoreStockCamera(bool checkMemory);
	static uint8_t clampQuality(uint8_t q);
};

#endif //BUTTERBOT_FIRMWARE_REMOTESESSION_H

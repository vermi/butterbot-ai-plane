#include "RemoteSession.h"
#include <cstring>
#include <esp_heap_caps.h>
#include <Devices/Camera.h>
#include <Services/Audio/Audio.h>
#include <Util/ServiceLocator.h>
#include <Util/stdafx.h>
#include <Log/Log.h>
#include <img_converters.h>
#include "LinkService.h"

DEFINE_LOG(RemoteSession)

RemoteSession::RemoteSession(LinkService* link, Camera* camera, Audio* audio) : link(link), camera(camera), audio(audio){
	stream = std::make_shared<PCMStream>();
	micBuf = static_cast<int16_t*>(heap_caps_malloc(MicChunk * sizeof(int16_t), MALLOC_CAP_SPIRAM));

	link->setSpeakerSink([s = stream](const uint8_t* pcm, size_t bytes){
		s->write(pcm, bytes);
	});

	camThread = std::make_unique<Threaded>([this](){ camLoop(); }, "RemoteCam", 0, 6 * 1024, 5, 1, false);
	micThread = std::make_unique<Threaded>([this](){ micLoop(); }, "RemoteMic", 0, 3 * 1024, 7, 1, false);
	camThread->start();
	micThread->start();
}

RemoteSession::~RemoteSession(){
	link->setSpeakerSink(nullptr);
	// Audio may still be playing the tail of a stream through `passthrough`, which dies with
	// this object: stop it (Audio::stop waits for an in-flight tick) whatever is playing.
	// Stop first, then end the stream, so playback does not also end itself mid-stop.
	if(audio && audio->isPlaying()){
		audio->stop();
	}
	stream->abort();
	speakerActive = false;

	camThread->stop(portMAX_DELAY);
	micThread->stop(portMAX_DELAY);

	if(snapshotPending){
		link->error(snapshotId, "left remote mode");
		snapshotPending = false;
	}

	// Wi-Fi is normally down by now (RemoteState suspends the link first); still check memory in
	// case it is not. Skipping is safe: stock users re-init the camera themselves.
	restoreStockCamera(true);
	free(micBuf);
}

uint8_t RemoteSession::clampQuality(uint8_t q){
	return std::clamp<uint8_t>(q, 4, 63);
}

void RemoteSession::setCamera(bool on, framesize_t res, uint8_t fps, uint8_t quality, bool swJpeg, uint8_t xclkMhz, int8_t psramDma){
	std::lock_guard lock(camMutex);
	camConfig = { .on = on, .res = res, .fps = std::clamp<uint8_t>(fps, 1, 30), .quality = clampQuality(quality),
				  .swJpeg = swJpeg, .xclkMhz = xclkMhz, .psramDma = psramDma };
}

void RemoteSession::requestSnapshot(uint32_t id, framesize_t res, uint8_t quality){
	std::lock_guard lock(camMutex);
	if(snapshotPending){
		link->error(snapshotId, "superseded by a newer snapshot");
	}
	snapshotPending = true;
	snapshotId = id;
	snapshotRes = res;
	snapshotQuality = clampQuality(quality);
}

void RemoteSession::setMic(bool on, bool gate, float gain){
	micGate = gate;
	micGain = gain;
	micOn = on;
}

bool RemoteSession::pause(bool p){
	paused = p;
	if(!p){
		return true;
	}
	// Blocks until the threads have finished their current hardware access (at most one
	// snapshot or one 128 ms mic read); they check `paused` under the same locks.
	std::lock_guard mic(micHw);
	std::lock_guard cam(camHw);
	return restoreStockCamera(true);
}

// ---------------------------------------------------------------- camera

bool RemoteSession::cameraFits(framesize_t res, bool jpeg, bool psramDma){
	// esp32-camera sizes a JPEG frame buffer at w*h/5 (RGB565: w*h*2) in PSRAM and needs one
	// 12-byte internal DMA descriptor per KB of it plus a 2 KB internal task stack. With Wi-Fi up
	// internal RAM is scarce, and a failed allocation aborts the firmware (heap-abort is enabled).
	const size_t pixels = (size_t) resolution[res].width * resolution[res].height;
	const size_t jpegMax = jpeg ? pixels / 5 : pixels * 2;
	const size_t descriptors = (jpegMax / 1024 + 1) * 12;
	const size_t freeInternal = heap_caps_get_free_size(MALLOC_CAP_INTERNAL | MALLOC_CAP_DMA);
	const size_t blockInternal = heap_caps_get_largest_free_block(MALLOC_CAP_INTERNAL | MALLOC_CAP_DMA);
	const size_t blockPsram = heap_caps_get_largest_free_block(MALLOC_CAP_SPIRAM);
	bool fits = blockInternal >= descriptors + 512 && freeInternal >= descriptors + 2048 + 4096
				&& blockPsram >= jpegMax * 2 + 64 * 1024; // frame buffer + the link's copy
	if(!psramDma){
		// Without PSRAM DMA the driver needs one contiguous internal DMA bounce buffer
		// (16 KB for sensor JPEG, up to CONFIG_CAMERA_DMA_BUFFER_SIZE_MAX for RGB).
		const size_t bounce = jpeg ? 16 * 1024 : CONFIG_CAMERA_DMA_BUFFER_SIZE_MAX;
		fits = fits && blockInternal >= bounce + 3 * 1024 && freeInternal >= bounce + 8 * 1024;
	}
	if(!fits){
		CMF_LOG(RemoteSession, LogLevel::Warning, "camera %ux%u does not fit: internal free %u block %u, psram block %u",
				resolution[res].width, resolution[res].height, (unsigned) freeInternal, (unsigned) blockInternal, (unsigned) blockPsram);
	}
	return fits;
}

bool RemoteSession::applyCapture(framesize_t res, uint8_t quality, const CamConfig& opts){
	if(!cameraFits(res, !opts.swJpeg, opts.psramDma != 0)){
		return false;
	}
	camera->setFormat(opts.swJpeg ? PIXFORMAT_RGB565 : PIXFORMAT_JPEG);
	camera->setRes(res);
	camera->setXclk(opts.xclkMhz * 1000000);
	if(opts.psramDma >= 0){
		camera->setPsramDma(opts.psramDma == 1);
	}
	if(camera->init() != ESP_OK){
		streamFormatActive = false;
		return false;
	}
	if(!opts.swJpeg){
		if(sensor_t* sensor = esp_camera_sensor_get()){
			sensor->set_quality(sensor, quality);
		}
	}
	lastQuality = quality;
	appliedSw = opts.swJpeg;
	appliedXclk = opts.xclkMhz;
	appliedDma = opts.psramDma;
	streamFormatActive = true;
	return true;
}

bool RemoteSession::captureAndSend(uint8_t quality, bool swJpeg, uint32_t seq, bool snapshot, uint32_t ackId){
	camera_fb_t* fb = camera->getFrame();
	if(fb == nullptr){
		camera->releaseFrame();
		return false;
	}
	bool ok = false;
	// A snapshot is acked only once its frame is ready (ack and frame share the ctrl queue).
	if(fb->format == PIXFORMAT_JPEG){
		if(ackId != 0){
			link->ack(ackId);
		}
		ok = link->sendVideo(fb->buf, fb->len, fb->width, fb->height, seq, snapshot);
	}else if(swJpeg){
		// Same path as the stock RC feed: encode the RGB565 frame on the CPU (output in PSRAM).
		// Map the link's 4..63 (lower is better) onto frame2jpg's 1..100 (higher is better);
		// capped so frames stay within frame2jpg's fixed 128 KB output buffer.
		const uint8_t q = (uint8_t) std::clamp<int>(100 - quality, 10, 80);
		uint8_t* out = nullptr;
		size_t len = 0;
		if(frame2jpg(fb, q, &out, &len) && out != nullptr){
			if(ackId != 0){
				link->ack(ackId);
			}
			ok = link->sendVideo(out, len, fb->width, fb->height, seq, snapshot);
		}
		free(out);
	}
	camera->releaseFrame();
	return ok;
}

bool RemoteSession::restoreStockCamera(bool checkMemory){
	// FaceDet/ObjDet and the RC feed expect an initialised 128x128 RGB565 camera. Camera::init()
	// is a no-op when it already is, so this is also right after a failed JPEG init.
	const bool ready = camera->isInited() && camera->getRes() == FRAMESIZE_128X128 && camera->getFormat() != PIXFORMAT_JPEG;
	// Always leave the stock settings pending, so whoever inits next gets 128x128 RGB565.
	camera->setFormat(PIXFORMAT_RGB565);
	camera->setRes(FRAMESIZE_128X128);
	camera->setXclk(0);
	camera->setPsramDma(true); // sdkconfig default (CONFIG_CAMERA_PSRAM_DMA)
	streamFormatActive = false;
	if(!ready && checkMemory && !cameraFits(FRAMESIZE_128X128, false)){
		camera->deinit(); // ~RemoteState re-inits it once Wi-Fi is down
		return false;
	}
	camera->init();
	return true;
}

void RemoteSession::camLoop(){
	if(paused){
		vTaskDelay(pdMS_TO_TICKS(50));
		return;
	}
	std::unique_lock hw(camHw);
	if(paused){
		return;
	}

	CamConfig cfg;
	bool snap;
	uint32_t snapId;
	framesize_t snapRes;
	uint8_t snapQuality;
	{
		std::lock_guard lock(camMutex);
		cfg = camConfig;
		snap = snapshotPending;
		snapId = snapshotId;
		snapRes = snapshotRes;
		snapQuality = snapshotQuality;
		snapshotPending = false;
	}

	if(snap){
		if(!cameraFits(snapRes, !cfg.swJpeg, cfg.psramDma != 0)){
			link->error(snapId, "not enough memory for this resolution while Wi-Fi is on; use a smaller one");
		}else if(!applyCapture(snapRes, snapQuality, cfg)){
			link->error(snapId, "camera init failed");
		}else{
			// The first frames after a re-init are often under-exposed or partial; skip a couple.
			for(int i = 0; i < 2; i++){
				camera->getFrame();
				camera->releaseFrame();
			}
			if(!captureAndSend(snapQuality, cfg.swJpeg, snapId, true, snapId)){
				link->error(snapId, "no frame from camera");
			}
		}
		// The live stream notices the change below and re-applies its own settings.
		return;
	}

	if(!cfg.on){
		hw.unlock();
		vTaskDelay(pdMS_TO_TICKS(50));
		return;
	}

	const uint64_t now = millis();
	if(now < nextFrameAt){
		hw.unlock();
		vTaskDelay(pdMS_TO_TICKS(std::min<uint64_t>(nextFrameAt - now, 50)));
		return;
	}
	nextFrameAt = now + 1000 / cfg.fps;

	// Camera de-initialises itself after repeated null frames, so check that too.
	const pixformat_t want = cfg.swJpeg ? PIXFORMAT_RGB565 : PIXFORMAT_JPEG;
	if(!streamFormatActive || !camera->isInited() || camera->getRes() != cfg.res || appliedSw != cfg.swJpeg
	   || appliedXclk != cfg.xclkMhz || appliedDma != cfg.psramDma
	   || (camera->getFormat() == PIXFORMAT_JPEG) != (want == PIXFORMAT_JPEG)){
		if(!applyCapture(cfg.res, cfg.quality, cfg)){
			CMF_LOG(RemoteSession, LogLevel::Error, "camera init failed for streaming");
			hw.unlock();
			vTaskDelay(pdMS_TO_TICKS(2000));
			return;
		}
	}else if(!cfg.swJpeg && cfg.quality != lastQuality){
		if(sensor_t* sensor = esp_camera_sensor_get()){
			sensor->set_quality(sensor, cfg.quality);
		}
		lastQuality = cfg.quality;
	}

	// Network slower than the camera: skip capturing rather than queueing stale frames.
	if(link->videoPending()){
		return;
	}

	captureAndSend(cfg.quality, cfg.swJpeg, ++frameSeq, false);
}

// ---------------------------------------------------------------- microphone

void RemoteSession::micLoop(){
	if(paused || !micOn || !ServiceLocator::MicInputInstance || micBuf == nullptr){
		vTaskDelay(pdMS_TO_TICKS(50));
		return;
	}

	int32_t n;
	{
		std::lock_guard hw(micHw);
		if(paused){
			return;
		}
		n = ServiceLocator::MicInputInstance->getMono({ micBuf, MicChunk });
	}
	if(n <= 0){
		vTaskDelay(pdMS_TO_TICKS(10));
		return;
	}

	// No echo cancellation: mute while the robot itself is talking.
	if(micGate && audio && audio->isPlaying()){
		memset(micBuf, 0, n * sizeof(int16_t));
	}else{
		const float gain = micGain;
		if(gain != 1.0f){
			for(int32_t i = 0; i < n; i++){
				micBuf[i] = (int16_t) std::clamp<float>(micBuf[i] * gain, INT16_MIN, INT16_MAX);
			}
		}
	}

	link->sendMic(micBuf, n, ++micSeq);
}

// ---------------------------------------------------------------- speaker

void RemoteSession::speakerBegin(){
	if(audio == nullptr){
		return;
	}
	stream->reset();
	speakerActive = true;
	audio->play(&passthrough, std::make_unique<PCMStreamSource>(stream));
}

void RemoteSession::speakerEnd(){
	stream->finish();
	speakerActive = false;
}

void RemoteSession::speakerAbort(){
	// Also covers the tail after spk_end, and a robot-voice utterance that is still playing.
	if(audio && audio->isPlaying()){
		audio->stop();
	}
	stream->abort();
	speakerActive = false;
}

bool RemoteSession::speakerStreaming() const{
	return speakerActive;
}

#ifndef BUTTERBOT_FIRMWARE_PCMSTREAM_H
#define BUTTERBOT_FIRMWARE_PCMSTREAM_H

#include <atomic>
#include <memory>
#include <freertos/FreeRTOS.h>
#include <freertos/ringbuf.h>
#include <Services/Audio/AudioSource.h>
#include <Services/Audio/AudioGenerator.h>

/**
 * Live 16 kHz mono PCM fed from the network into the speaker. The writer (link thread) pushes
 * bytes; the reader (audio thread) pulls them through PCMStreamSource. While the stream is open
 * and empty the reader gets silence, so Audio keeps playing across network jitter; it only sees
 * end-of-data after finish() and once everything buffered has been played.
 */
class PCMStream {
public:
	explicit PCMStream(size_t bytes = 64 * 1024);
	~PCMStream();

	/** Start a new utterance: drop leftovers and accept data again. */
	void reset();
	/** Append PCM. Returns false (and drops it) if the buffer is full. */
	bool write(const uint8_t* data, size_t len);
	/** No more data will follow. */
	void finish();
	/** Drop everything buffered and end the stream now. */
	void abort();

	size_t read(uint8_t* out, size_t len, TickType_t wait);
	bool finished() const { return done; }
	uint32_t underruns() const { return underrunCount; }
	size_t buffered() const;

	std::atomic_uint32_t underrunCount = 0;

private:
	RingbufHandle_t rb = nullptr;
	std::atomic_bool done = true;
	void drain();
};

class PCMStreamSource : public AudioSource {
public:
	explicit PCMStreamSource(std::shared_ptr<PCMStream> stream) : stream(std::move(stream)){}

	void open() override{}
	void close() override{}
	size_t getData(uint8_t* buffer, size_t bytes) override;
	operator bool() const override{ return stream != nullptr; }

private:
	std::shared_ptr<PCMStream> stream;
	bool sawData = false;
};

/** Hands source bytes straight to Audio: no decoding, compression or effects. */
class PassthroughGen : public AudioGenerator {
public:
	void open(std::unique_ptr<AudioSource> resource) override;
	void close() override;
	size_t getData(uint8_t* buffer, size_t bytes) override;

private:
	std::unique_ptr<AudioSource> source;
};

#endif //BUTTERBOT_FIRMWARE_PCMSTREAM_H

#include "PCMStream.h"
#include <cstring>
#include <esp_heap_caps.h>

PCMStream::PCMStream(size_t bytes){
	rb = xRingbufferCreateWithCaps(bytes, RINGBUF_TYPE_BYTEBUF, MALLOC_CAP_SPIRAM);
}

PCMStream::~PCMStream(){
	if(rb){
		vRingbufferDeleteWithCaps(rb);
	}
}

void PCMStream::drain(){
	size_t size = 0;
	while(void* item = xRingbufferReceive(rb, &size, 0)){
		vRingbufferReturnItem(rb, item);
	}
}

void PCMStream::reset(){
	drain();
	underrunCount = 0;
	done = false;
}

bool PCMStream::write(const uint8_t* data, size_t len){
	if(rb == nullptr || done){
		return false;
	}
	return xRingbufferSend(rb, data, len, 0) == pdTRUE;
}

void PCMStream::finish(){
	done = true;
}

void PCMStream::abort(){
	done = true;
	drain();
}

size_t PCMStream::buffered() const{
	if(rb == nullptr){
		return 0;
	}
	UBaseType_t waiting = 0;
	vRingbufferGetInfo(rb, nullptr, nullptr, nullptr, nullptr, &waiting);
	return waiting;
}

size_t PCMStream::read(uint8_t* out, size_t len, TickType_t wait){
	if(rb == nullptr){
		return 0;
	}
	size_t total = 0;
	// A byte buffer may hand back the data in two pieces when it wraps.
	while(total < len){
		size_t got = 0;
		void* item = xRingbufferReceiveUpTo(rb, &got, total == 0 ? wait : 0, len - total);
		if(item == nullptr){
			break;
		}
		memcpy(out + total, item, got);
		vRingbufferReturnItem(rb, item);
		total += got;
	}
	return total & ~(size_t) 1; // whole samples only
}

size_t PCMStreamSource::getData(uint8_t* buffer, size_t bytes){
	size_t got = stream->read(buffer, bytes, pdMS_TO_TICKS(60));
	if(got > 0){
		sawData = true;
		return got;
	}
	if(stream->finished()){
		return 0; // ends playback
	}
	// Network jitter: keep the I2S fed with silence instead of stopping.
	if(sawData){
		stream->underrunCount++;
	}
	memset(buffer, 0, bytes);
	return bytes;
}

void PassthroughGen::open(std::unique_ptr<AudioSource> resource){
	source = std::move(resource);
	if(source){
		source->open();
	}
}

void PassthroughGen::close(){
	if(source){
		source->close();
		source.reset();
	}
}

size_t PassthroughGen::getData(uint8_t* buffer, size_t bytes){
	return source ? source->getData(buffer, bytes) : 0;
}

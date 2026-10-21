#include "PeekView.h"

#include <cstring>
#include <esp_heap_caps.h>
#include <BBData.h>
#include "Fonts/font.hpp"

PeekView::PeekView(lv_obj_t* parent) : LVObject(parent){
	theme = Application::getApp()->getService<ThemeService>();

	lv_obj_set_size(*this, 128, 120);
	lv_obj_set_pos(*this, 0, 8);
	lv_obj_remove_flag(*this, LV_OBJ_FLAG_SCROLLABLE);
	lv_obj_set_style_bg_color(*this, theme->getTertiaryColor(), 0);
	lv_obj_set_style_bg_opa(*this, LV_OPA_COVER, 0);
	lv_obj_set_style_pad_all(*this, 0, 0);
	lv_obj_set_style_border_width(*this, 0, 0);
	lv_obj_set_style_radius(*this, 0, 0);

	// esp_new_jpeg wants a 16-byte aligned output buffer
	pixels = (uint8_t*) heap_caps_aligned_alloc(16, W * H * 2, MALLOC_CAP_SPIRAM);
	if(pixels != nullptr){
		memset(pixels, 0, W * H * 2);
	}
	dsc.header.magic = LV_IMAGE_HEADER_MAGIC;
	dsc.header.cf = LV_COLOR_FORMAT_RGB565;
	dsc.header.w = W;
	dsc.header.h = H;
	dsc.header.stride = W * 2;
	dsc.data_size = W * H * 2;
	dsc.data = pixels;

	img = lv_image_create(*this);
	lv_obj_set_pos(img, 0, 2);
	lv_obj_add_flag(img, LV_OBJ_FLAG_HIDDEN);
	if(pixels != nullptr){
		lv_image_set_src(img, &dsc);
	}

	status = lv_label_create(*this);
	lv_obj_set_style_text_font(status, &lv_font_butter, 0);
	lv_obj_set_style_text_color(status, theme->getPrimaryColor(), 0);
	lv_label_set_text_static(status, "Looking...");
	lv_obj_align(status, LV_ALIGN_TOP_MID, 0, 44);

	caption = lv_label_create(*this);
	lv_obj_set_style_text_font(caption, &lv_font_butter, 0);
	lv_obj_set_style_text_color(caption, theme->getPrimaryColor(), 0);
	lv_obj_set_style_text_align(caption, LV_TEXT_ALIGN_CENTER, 0);
	lv_label_set_long_mode(caption, LV_LABEL_LONG_SCROLL_CIRCULAR);
	lv_obj_set_size(caption, 124, 8);
	lv_obj_set_pos(caption, 2, H + 6);
	lv_label_set_text(caption, "");

	jpeg_dec_config_t config = DEFAULT_JPEG_DEC_CONFIG();
	config.output_type = JPEG_PIXEL_FORMAT_RGB565_LE;
	config.rotate = JPEG_ROTATE_0D;
	if(jpeg_dec_open(&config, &dec) != JPEG_ERR_OK){
		dec = nullptr;
	}
}

PeekView::~PeekView(){
	if(dec != nullptr){
		jpeg_dec_close(dec);
	}
	if(pixels != nullptr){
		heap_caps_free(pixels);
	}
}

void PeekView::onCaption(const std::vector<uint8_t>& data){
	const PeekCaptionData d = PeekCaptionData::deserialize(data.data(), data.size());
	if(strcmp(lv_label_get_text(caption), d.text.c_str()) != 0){
		lv_label_set_text(caption, d.text.c_str());
	}
}

// Pieces of one picture arrive in order but may go missing (the radio is busy): a picture is
// shown only once all its pieces are in; one with a gap is dropped when the next one starts.
void PeekView::onChunk(const std::vector<uint8_t>& data){
	if(data.size() <= sizeof(PeekChunkHeader)){
		return;
	}
	PeekChunkHeader h{};
	memcpy(&h, data.data(), sizeof(h));
	if(h.count == 0 || h.index >= h.count){
		return;
	}
	if(h.frame != frame || got.size() != h.count){
		frame = h.frame;
		got.assign(h.count, false);
		gotCount = 0;
		jpeg.assign((size_t) h.count * PeekChunkHeader::MaxBytes, 0);
	}
	if(got[h.index]){
		return;
	}
	const size_t len = data.size() - sizeof(h);
	if(len > PeekChunkHeader::MaxBytes){
		return;
	}
	memcpy(jpeg.data() + (size_t) h.index * PeekChunkHeader::MaxBytes, data.data() + sizeof(h), len);
	got[h.index] = true;
	gotCount++;
	if(h.index == h.count - 1){
		// The last piece is short: the picture ends there.
		jpeg.resize((size_t) h.index * PeekChunkHeader::MaxBytes + len);
	}
	if(gotCount == h.count){
		decode();
		frame = -1;
		got.clear();
	}
}

void PeekView::decode(){
	if(dec == nullptr || pixels == nullptr){
		return;
	}
	jpeg_dec_io_t io{};
	jpeg_dec_header_info_t info{};
	io.inbuf = jpeg.data();
	io.inbuf_len = (int) jpeg.size();
	if(jpeg_dec_parse_header(dec, &io, &info) != JPEG_ERR_OK || info.width != W || info.height != H){
		return;
	}
	io.outbuf = pixels;
	if(jpeg_dec_process(dec, &io) != JPEG_ERR_OK){
		return;
	}
	lv_obj_add_flag(status, LV_OBJ_FLAG_HIDDEN);
	lv_obj_remove_flag(img, LV_OBJ_FLAG_HIDDEN);
	lv_obj_invalidate(img);
}

// KMS/DMA-BUF zero-copy capture and VAAPI encode for IP-KVM OS Services.
//
// Scanout DMA-BUFs are imported on the capture GPU. An owned EGL surface
// adds the hardware cursor; VAAPI converts to NV12 and encodes the result.
// Desktop pixels never pass through CPU memory. kms_capture.cpp holds the
// capture itself. The driver's scanout format must be one VAAPI can import
// (Intel iHD: yes). A driver without an H.264 encoder, such as Fedora's
// Mesa, still converts: the NV12 frame then comes to memory and OpenH264
// encodes it (openh264.h).

#include <unistd.h>

#include <chrono>
#include <cstdint>
#include <cstring>
#include <memory>
#include <string>

extern "C" {
#include <drm_fourcc.h>
#include <libavcodec/avcodec.h>
#include <libavfilter/avfilter.h>
#include <libavfilter/buffersink.h>
#include <libavfilter/buffersrc.h>
#include <libavutil/error.h>
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_drm.h>
#include <libavutil/hwcontext_vaapi.h>
#include <libavutil/opt.h>
#include <libavutil/pixdesc.h>
#include <va/va_backend.h>
}

#include "kms_capture.h"

#define LOG_MODULE "KMSVAAPI"
#include "log.h"
#include "cursor_vaapi.h"
#include "openh264.h"

namespace {

// The fourcc that the PRIME frame gives FFmpeg for a scanout format. KWin
// scans out 10-bit desktops as ARGB2101010 or ABGR2101010, which FFmpeg's
// DRM-to-VAAPI mapping lacks; the X formats have the same layout, and the
// conversion to NV12 ignores alpha.
uint32_t prime_format(uint32_t fourcc) {
  switch (fourcc) {
  case DRM_FORMAT_ARGB2101010:
    return DRM_FORMAT_XRGB2101010;
  case DRM_FORMAT_ABGR2101010:
    return DRM_FORMAT_XBGR2101010;
  default:
    return fourcc;
  }
}

// The scanout's DRM fourcc as the software format behind the PRIME frame,
// which is what hwmap needs to derive the VAAPI surface.
AVPixelFormat drm_pixel_format(uint32_t fourcc) {
  switch (prime_format(fourcc)) {
  case DRM_FORMAT_XRGB8888:
    return AV_PIX_FMT_BGR0;
  case DRM_FORMAT_ARGB8888:
    return AV_PIX_FMT_BGRA;
  case DRM_FORMAT_XBGR8888:
    return AV_PIX_FMT_RGB0;
  case DRM_FORMAT_ABGR8888:
    return AV_PIX_FMT_RGBA;
  case DRM_FORMAT_RGB565:
    return AV_PIX_FMT_RGB565LE;
  case DRM_FORMAT_XRGB2101010:
    return AV_PIX_FMT_X2RGB10LE;
  case DRM_FORMAT_XBGR2101010:
    return AV_PIX_FMT_X2BGR10LE;
  case DRM_FORMAT_NV12:
    return AV_PIX_FMT_NV12;
  default:
    return AV_PIX_FMT_NONE;
  }
}

struct KmsEncoder {
  KmsCapture capture;
  AVBufferRef *device_ref = nullptr;
  AVBufferRef *frames_ref = nullptr;
  AVFilterGraph *graph = nullptr;
  AVFilterContext *source = nullptr;
  AVFilterContext *sink = nullptr;
  AVCodecContext *encoder = nullptr;
  AVPacket *packet = nullptr;
  AVFrame *prime_frame = nullptr;
  AVFrame *vaapi_frame = nullptr;
  AVFrame *composited_frame = nullptr;
  CursorVpp cursor;
  // The software encoder when VAAPI has no H.264 encoder, and the frame in
  // memory it reads.
  std::unique_ptr<Openh264> software;
  AVFrame *download = nullptr;
  int kbs = 0;
  int fps = 0;
  int gop = 0;
  std::string encoder_name;
  // Rate control set after open by kms_vaapi_set_rate, which every later
  // picture carries.
  bool retargeted = false;
  bool rate_rejected = false;
  VADisplay display = nullptr;
  VAEncMiscParameterRateControl rc_params = {};
  VAEncMiscParameterHRD hrd_params = {};
  VAEncMiscParameterFrameRate fr_params = {};
  VAStatus (*end_picture)(VADriverContextP, VAContextID) = nullptr;
};

void destroy(KmsEncoder *self) {
  if (!self)
    return;
  avcodec_free_context(&self->encoder);
  avfilter_graph_free(&self->graph);
  av_packet_free(&self->packet);
  av_frame_free(&self->prime_frame);
  av_frame_free(&self->vaapi_frame);
  av_frame_free(&self->composited_frame);
  av_frame_free(&self->download);
  av_buffer_unref(&self->frames_ref);
  av_buffer_unref(&self->device_ref);
  self->capture.close();
  delete self;
}

void free_prime_descriptor(void *, uint8_t *data) {
  auto desc = reinterpret_cast<AVDRMFrameDescriptor *>(data);
  for (int i = 0; i < desc->nb_objects; i++)
    close(desc->objects[i].fd);
  av_free(desc);
}

// A PRIME frames context and the filter graph and encoder behind it are
// built off the first captured frame, whose format and modifier they carry.
KmsOpenFailure build_frames(KmsEncoder *self, const drmtap_dmabuf_desc &desc) {
  AVPixelFormat sw_format = drm_pixel_format(desc.format);
  if (sw_format == AV_PIX_FMT_NONE) {
    LOG_ERROR("scanout format " + std::to_string(desc.format) +
              " has no VAAPI mapping");
    return KMS_FORMAT_UNSUPPORTED;
  }
  const char *node = self->capture.lib->render_node(self->capture.ctx);
  int ret = av_hwdevice_ctx_create(&self->device_ref, AV_HWDEVICE_TYPE_DRM,
                                   node, nullptr, 0);
  // A denied render node is reported by its failure code alone: the probes
  // retry every few seconds while it lasts, and the caller logs the reason
  // once.
  if (ret == AVERROR(EACCES) || ret == AVERROR(EPERM))
    return KMS_RENDER_NODE_DENIED;
  if (ret < 0) {
    log_av_error("DRM device open", ret);
    return KMS_PIPELINE_FAILED;
  }
  self->frames_ref = av_hwframe_ctx_alloc(self->device_ref);
  auto frames = reinterpret_cast<AVHWFramesContext *>(self->frames_ref->data);
  frames->format = AV_PIX_FMT_DRM_PRIME;
  frames->sw_format = sw_format;
  frames->width = int(desc.width);
  frames->height = int(desc.height);
  if ((ret = av_hwframe_ctx_init(self->frames_ref)) < 0) {
    log_av_error("PRIME frames init", ret);
    return KMS_PIPELINE_FAILED;
  }
  return self->capture.adopt(desc) ? KMS_OPENED : KMS_PIPELINE_FAILED;
}

// H.264 with OpenH264, for a driver without an H.264 encoder.
KmsOpenFailure open_software(KmsEncoder *self) {
  self->fps = std::min(self->fps, Openh264::kMaxFps);
  auto software = std::make_unique<Openh264>();
  if (!software->open(self->capture.width, self->capture.height, self->fps, self->kbs))
    return KMS_ENCODER_UNAVAILABLE;
  self->software = std::move(software);
  self->encoder_name = "openh264";
  return KMS_OPENED;
}

KmsOpenFailure build_graph(KmsEncoder *self) {
  int ret;
  self->graph = avfilter_graph_alloc();
  if (!self->graph)
    return KMS_PIPELINE_FAILED;

  // A source of hardware frames gets its frames context before it
  // initializes; FFmpeg refuses a hardware format without one.
  self->source = avfilter_graph_alloc_filter(self->graph, avfilter_get_by_name("buffer"), "in");
  AVBufferSrcParameters *params = av_buffersrc_parameters_alloc();
  if (!self->source || !params) {
    av_free(params);
    return KMS_PIPELINE_FAILED;
  }
  params->format = AV_PIX_FMT_DRM_PRIME;
  params->width = self->capture.width;
  params->height = self->capture.height;
  params->time_base = av_make_q(1, 1000000);
  params->hw_frames_ctx = self->frames_ref;
  ret = av_buffersrc_parameters_set(self->source, params);
  av_free(params);
  if (ret >= 0)
    ret = avfilter_init_dict(self->source, nullptr);
  if (ret < 0) {
    LOG_ERROR("buffersrc create failed: " + std::to_string(ret));
    return KMS_PIPELINE_FAILED;
  }
  ret = avfilter_graph_create_filter(&self->sink,
                                     avfilter_get_by_name("buffersink"),
                                     "out", nullptr, nullptr, self->graph);
  if (ret < 0)
    return KMS_PIPELINE_FAILED;

  AVFilterInOut *outputs = avfilter_inout_alloc();
  AVFilterInOut *inputs = avfilter_inout_alloc();
  outputs->name = av_strdup("in");
  outputs->filter_ctx = self->source;
  outputs->pad_idx = 0;
  outputs->next = nullptr;
  inputs->name = av_strdup("out");
  inputs->filter_ctx = self->sink;
  inputs->pad_idx = 0;
  inputs->next = nullptr;
  ret = avfilter_graph_parse_ptr(
      self->graph, "hwmap=derive_device=vaapi",
      &inputs, &outputs, nullptr);
  avfilter_inout_free(&inputs);
  avfilter_inout_free(&outputs);
  if (ret < 0) {
    LOG_ERROR("graph parse failed: " + std::to_string(ret));
    return KMS_PIPELINE_FAILED;
  }
  if ((ret = avfilter_graph_config(self->graph, nullptr)) < 0) {
    LOG_ERROR("graph config failed: " + std::to_string(ret));
    return KMS_PIPELINE_FAILED;
  }

  if (self->cursor.init(av_buffersink_get_hw_frames_ctx(self->sink),
                         self->capture.width, self->capture.height,
                         self->capture.lib->render_node(self->capture.ctx)) < 0)
    return KMS_PIPELINE_FAILED;

  const AVCodec *codec =
      avcodec_find_encoder_by_name(self->encoder_name.c_str());
  if (!codec) {
    LOG_ERROR(self->encoder_name + " encoder not available");
    return KMS_ENCODER_UNAVAILABLE;
  }
  // avcodec_open2 clears hw_frames_ctx on failure. Rebuild the context
  // before retrying drivers that expose only the full-power entrypoint.
  for (int low_power : {1, 0}) {
    self->encoder = avcodec_alloc_context3(codec);
    self->encoder->width = self->capture.width;
    self->encoder->height = self->capture.height;
    self->encoder->pix_fmt = AV_PIX_FMT_VAAPI;
    self->encoder->time_base = av_make_q(1, 1000000);
    self->encoder->framerate = av_make_q(self->fps, 1);
    self->encoder->bit_rate = int64_t(self->kbs) * 1000;
    self->encoder->gop_size = self->gop;
    self->encoder->keyint_min = self->gop;
    self->encoder->max_b_frames = 0;
    self->encoder->flags |= AV_CODEC_FLAG_LOW_DELAY;
    self->encoder->color_range = AVCOL_RANGE_MPEG;
    self->encoder->colorspace = AVCOL_SPC_SMPTE170M;
    self->encoder->color_primaries = AVCOL_PRI_SMPTE170M;
    self->encoder->color_trc = AVCOL_TRC_SMPTE170M;
    self->encoder->hw_frames_ctx =
        av_buffer_ref(self->cursor.frames);
    av_opt_set(self->encoder->priv_data, "async_depth", "1", 0);
    av_opt_set(self->encoder->priv_data, "low_power", low_power ? "1" : "0", 0);
    // VBR with a generous peak: this path encodes every scanout frame, so a
    // constant-rate budget starves motion (~6 KB/frame at 6 Mbit 120 fps) and
    // wastes bits padding static frames. The average targets kbs; bursts may
    // use up to 2.5x during scrolling/motion.
    av_opt_set(self->encoder->priv_data, "rc_mode", "VBR", 0);
    self->encoder->rc_max_rate = peak_bit_rate(self->kbs);
    // Target usage runs from 1 (best) to 7 (fastest). For H.264, level 7 is
    // visibly soft at 1080p; level 2 adds 1-2 ms of GPU encode time on a
    // low-power GPU, well inside the 8.3 ms frame interval at 120 fps.
    // H.265 keeps the driver's default: at levels 1 and 2, Intel's iHD
    // driver built without its non-free kernels, the distributions' default
    // package, encodes inter pictures that do not match the bitstream, and
    // the picture smears until the next IDR.
    if (codec->id == AV_CODEC_ID_H264)
      self->encoder->compression_level = 2;

    ret = avcodec_open2(self->encoder, codec, nullptr);
    if (ret >= 0)
      break;
    avcodec_free_context(&self->encoder);
  }
  if (ret < 0) {
    LOG_ERROR(self->encoder_name + " open failed: " + std::to_string(ret));
    return self->encoder_name == "h264_vaapi" ? open_software(self) : KMS_ENCODER_UNAVAILABLE;
  }
  return KMS_OPENED;
}

// A new rate for the running encoder. FFmpeg's VAAPI encoder fixes its rate
// control at open and sends it only with IDR pictures. VAAPI drivers take
// rate-control, HRD and frame-rate parameters with any picture, and reset
// their rate control when the values change. So while a retargeted encoder
// encodes, its display's vaEndPicture runs end_picture_retargeted, which adds
// the new parameters to the picture FFmpeg submits. Every later picture
// carries them: an IDR also carries the parameters of the open, and FFmpeg
// renders those first, so the new ones win.
thread_local KmsEncoder *retargeting = nullptr;

// The VAAPI display the encoder runs on.
VADisplay encoder_display(KmsEncoder *self) {
  auto frames =
      reinterpret_cast<AVHWFramesContext *>(self->encoder->hw_frames_ctx->data);
  return static_cast<AVVAAPIDeviceContext *>(frames->device_ctx->hwctx)->display;
}

// The driver function table behind a display, which libva calls for each
// VA call on it.
VADriverVTable *driver_table(VADisplay display) {
  auto context = static_cast<VADisplayContextP>(display);
  if (!context || !context->pDriverContext)
    return nullptr;
  return context->pDriverContext->vtable;
}

// The parameters FFmpeg's VBR mode derives at open from bit_rate and
// rc_max_rate (vaapi_encode_init_rate_control), for the current rate.
void set_rate_params(KmsEncoder *self) {
  const int64_t peak = peak_bit_rate(self->kbs);
  self->rc_params = {};
  self->rc_params.bits_per_second = uint32_t(peak);
  self->rc_params.target_percentage =
      uint32_t(int64_t(self->kbs) * 1000 * 100 / peak);
  self->rc_params.window_size = 1000;
  self->rc_params.ICQ_quality_factor = 1;
  self->hrd_params = {};
  self->hrd_params.buffer_size = uint32_t(peak);
  self->hrd_params.initial_buffer_fullness = uint32_t(peak * 3 / 4);
  self->fr_params = {};
  self->fr_params.framerate = (1u << 16) | uint32_t(self->fps);
}

template <typename T>
bool make_misc_buffer(VADisplay display, VAContextID context,
                      VAEncMiscParameterType type, const T &params,
                      VABufferID *buffer) {
  VAEncMiscParameterBuffer header = {};
  header.type = type;
  uint8_t data[sizeof(header) + sizeof(params)];
  memcpy(data, &header, sizeof(header));
  memcpy(data + sizeof(header), &params, sizeof(params));
  VAStatus status = vaCreateBuffer(display, context, VAEncMiscParameterBufferType,
                                   sizeof(data), 1, data, buffer);
  if (status == VA_STATUS_SUCCESS)
    return true;
  LOG_ERROR(std::string("rate parameter buffer: ") + vaErrorStr(status));
  return false;
}

VAStatus end_picture_retargeted(VADriverContextP driver, VAContextID context) {
  KmsEncoder *self = retargeting;
  VADisplay display = self->display;
  VABufferID buffers[3] = {VA_INVALID_ID, VA_INVALID_ID, VA_INVALID_ID};
  bool rendered =
      make_misc_buffer(display, context, VAEncMiscParameterTypeRateControl,
                       self->rc_params, &buffers[0]) &&
      make_misc_buffer(display, context, VAEncMiscParameterTypeHRD,
                       self->hrd_params, &buffers[1]) &&
      make_misc_buffer(display, context, VAEncMiscParameterTypeFrameRate,
                       self->fr_params, &buffers[2]);
  if (rendered) {
    VAStatus status = vaRenderPicture(display, context, buffers, 3);
    rendered = status == VA_STATUS_SUCCESS;
    if (!rendered)
      LOG_ERROR(std::string("rate parameters refused: ") + vaErrorStr(status));
  }
  VAStatus status = self->end_picture(driver, context);
  if (rendered && status != VA_STATUS_SUCCESS)
    LOG_ERROR(std::string("picture with rate parameters refused: ") +
              vaErrorStr(status));
  // The encoder built at the new rate replaces this one, so a picture the
  // driver refused with these parameters does not end the video.
  if (!rendered || status != VA_STATUS_SUCCESS)
    self->rate_rejected = true;
  for (VABufferID buffer : buffers) {
    if (buffer != VA_INVALID_ID)
      vaDestroyBuffer(display, buffer);
  }
  return status;
}

// Points the display's vaEndPicture at end_picture_retargeted for as long as
// the encoder runs, when the encoder has been retargeted. libva gives each
// display its own table, and only the encoder ends pictures on this display
// meanwhile.
class RetargetScope {
 public:
  explicit RetargetScope(KmsEncoder *self) {
    if (!self->retargeted || self->rate_rejected)
      return;
    table = driver_table(self->display);
    self->end_picture = table->vaEndPicture;
    table->vaEndPicture = end_picture_retargeted;
    retargeting = self;
  }
  ~RetargetScope() {
    if (!table)
      return;
    table->vaEndPicture = retargeting->end_picture;
    retargeting = nullptr;
  }
  RetargetScope(const RetargetScope &) = delete;
  RetargetScope &operator=(const RetargetScope &) = delete;

 private:
  VADriverVTable *table = nullptr;
};

// Wraps the grabbed scanout as a PRIME frame. The frame gets its own
// duplicate of the dma-buf fd, so libdrmtap's frame can be released at once
// and the buffer lives as long as the filter graph holds the frame.
bool wrap_prime_frame(KmsEncoder *self, const drmtap_dmabuf_desc &desc,
                      AVFrame *frame) {
  int fd = dup(desc.dma_buf_fd);
  if (fd < 0) {
    LOG_ERROR("dma-buf fd dup failed");
    return false;
  }
  size_t size = self->capture.buffer_size;
  auto prime = static_cast<AVDRMFrameDescriptor *>(
      av_mallocz(sizeof(AVDRMFrameDescriptor)));
  prime->nb_objects = 1;
  prime->objects[0].fd = fd;
  prime->objects[0].size = size;
  prime->objects[0].format_modifier = desc.modifier;
  prime->nb_layers = 1;
  prime->layers[0].format = prime_format(desc.format);
  int planes = int(desc.num_planes ? desc.num_planes : 1);
  if (planes > AV_DRM_MAX_PLANES)
    planes = AV_DRM_MAX_PLANES;
  prime->layers[0].nb_planes = planes;
  for (int i = 0; i < planes; i++) {
    prime->layers[0].planes[i].object_index = 0;
    prime->layers[0].planes[i].offset = desc.offsets[i];
    prime->layers[0].planes[i].pitch = desc.pitches[i];
  }
  frame->buf[0] = av_buffer_create(reinterpret_cast<uint8_t *>(prime),
                                   sizeof(*prime), free_prime_descriptor,
                                   nullptr, 0);
  if (!frame->buf[0]) {
    free_prime_descriptor(nullptr, reinterpret_cast<uint8_t *>(prime));
    return false;
  }
  frame->data[0] = reinterpret_cast<uint8_t *>(prime);
  frame->format = AV_PIX_FMT_DRM_PRIME;
  frame->width = self->capture.width;
  frame->height = self->capture.height;
  frame->hw_frames_ctx = av_buffer_ref(self->frames_ref);
  frame->pts = std::chrono::duration_cast<std::chrono::microseconds>(
                   std::chrono::steady_clock::now().time_since_epoch())
                   .count();
  return true;
}

// Brings the composited NV12 frame to memory and encodes it with OpenH264.
int32_t encode_in_software(KmsEncoder *self, int32_t force_idr, uint64_t pts_us,
                           KmsPacketCallback callback, const void *obj) {
  self->download->format = AV_PIX_FMT_NV12;
  int ret = av_hwframe_transfer_data(self->download, self->composited_frame, 0);
  av_frame_unref(self->composited_frame);
  if (ret < 0) {
    log_av_error("frame download", ret);
    return -7;
  }
  ret = self->software->encode(self->download, force_idr != 0, pts_us,
                               [&](const uint8_t *data, int size, bool key) {
                                 callback(data, size, key ? 1 : 0, pts_us, obj);
                               });
  av_frame_unref(self->download);
  if (ret != 0) {
    LOG_ERROR("OpenH264 encode failed: " + std::to_string(ret));
    return -8;
  }
  return KMS_PULL_ENCODED;
}

} // namespace

extern "C" {

// Opens a capture on `device` and builds the whole pipeline behind it. On
// failure returns nullptr and stores why in `failure` (a KmsOpenFailure).
void *kms_vaapi_new_encoder(const char *device, uint32_t connector_id, const char *encoder_name,
                            int32_t framerate, int32_t kbs, int32_t gop,
                            int32_t *failure) {
  auto self = new KmsEncoder();
  self->kbs = kbs;
  self->fps = framerate;
  self->gop = gop;
  self->encoder_name =
      (encoder_name && encoder_name[0]) ? encoder_name : "h264_vaapi";

  KmsFrame first;
  *failure = self->capture.open(device, connector_id, first);
  if (*failure != KMS_OPENED) {
    destroy(self);
    return nullptr;
  }
  // The scanout's geometry, format and modifier fix the whole pipeline, so
  // everything is built here rather than off the first streamed frame: an
  // encoder this machine cannot open should fail the open, not frame one.
  *failure = build_frames(self, first.desc);
  if (*failure == KMS_OPENED)
    *failure = build_graph(self);
  self->capture.release(first);
  if (*failure != KMS_OPENED) {
    destroy(self);
    return nullptr;
  }
  self->packet = av_packet_alloc();
  self->prime_frame = av_frame_alloc();
  self->vaapi_frame = av_frame_alloc();
  self->composited_frame = av_frame_alloc();
  self->download = av_frame_alloc();
  self->capture.start_pacing(self->fps);
  return self;
}

int32_t kms_vaapi_dimensions(void *handle, int32_t *width, int32_t *height) {
  auto self = static_cast<KmsEncoder *>(handle);
  *width = self->capture.width;
  *height = self->capture.height;
  return 0;
}

// Retargets the running encoder to `kbs` at `framerate` and keeps its
// stream: the pacing changes at once, and the next picture carries the new
// rate control. Returns 0, or -1 when the encoder cannot take a new rate and
// the caller must build a new one.
int32_t kms_vaapi_set_rate(void *handle, int32_t framerate, int32_t kbs) {
  auto self = static_cast<KmsEncoder *>(handle);
  if (self->software) {
    if (framerate <= 0 || kbs <= 0)
      return -1;
    self->kbs = kbs;
    self->fps = std::min(framerate, Openh264::kMaxFps);
    self->capture.set_pacing(self->fps);
    self->software->set_rate(self->fps, kbs);
    return 0;
  }
  VADisplay display = encoder_display(self);
  VADriverVTable *table = driver_table(display);
  if (framerate <= 0 || kbs <= 0 || self->rate_rejected || !table ||
      !table->vaEndPicture)
    return -1;
  self->display = display;
  self->kbs = kbs;
  self->fps = framerate;
  // A higher rate takes effect at the next pull.
  self->capture.set_pacing(framerate);
  set_rate_params(self);
  self->retargeted = true;
  return 0;
}

int32_t kms_vaapi_next(void *handle, KmsPacketCallback callback,
                       const void *obj, int32_t force_idr) {
  auto self = static_cast<KmsEncoder *>(handle);
  if (self->rate_rejected)
    return KMS_PULL_RATE_REJECTED;

  self->capture.wait_for_frame();
  KmsFrame grabbed;
  int32_t pull = self->capture.grab(grabbed);
  if (pull != KMS_PULL_ENCODED)
    return pull;
  const bool other_modifier = self->capture.other_modifier();
  bool wrapped = wrap_prime_frame(self, grabbed.desc, self->prime_frame);
  self->capture.release(grabbed);
  if (!wrapped)
    return -2;

  uint64_t pts_us = uint64_t(self->prime_frame->pts);
  int ret = av_buffersrc_add_frame_flags(self->source, self->prime_frame, AV_BUFFERSRC_FLAG_KEEP_REF);
  if (ret < 0) {
    log_av_error("filter input", ret);
    return other_modifier ? KMS_PULL_SCANOUT_CHANGED : -5;
  }
  ret = av_buffersink_get_frame(self->sink, self->vaapi_frame);
  if (ret < 0) {
    if (ret != AVERROR(EAGAIN))
      log_av_error("filter output", ret);
    return ret == AVERROR(EAGAIN) ? KMS_PULL_IDLE
           : other_modifier       ? KMS_PULL_SCANOUT_CHANGED
                                  : -6;
  }

  drmtap_cursor_info cursor = {};
  ret = self->capture.lib->get_cursor(self->capture.ctx, &cursor);
  if (ret >= 0)
    ret = self->cursor.compose(self->vaapi_frame, self->prime_frame, self->composited_frame, cursor);
  self->capture.lib->cursor_release(self->capture.ctx, &cursor);
  av_frame_unref(self->prime_frame);
  av_frame_unref(self->vaapi_frame);
  if (ret < 0) {
    log_av_error("cursor composition", ret);
    return other_modifier ? KMS_PULL_SCANOUT_CHANGED : -6;
  }
  if (self->software)
    return encode_in_software(self, force_idr, pts_us, callback, obj);
  self->composited_frame->pict_type =
      force_idr ? AV_PICTURE_TYPE_I : AV_PICTURE_TYPE_NONE;
  RetargetScope retarget(self);
  ret = avcodec_send_frame(self->encoder, self->composited_frame);
  av_frame_unref(self->composited_frame);
  if (ret < 0) {
    log_av_error("encoder submit", ret);
    return self->rate_rejected ? KMS_PULL_RATE_REJECTED : -7;
  }
  while ((ret = avcodec_receive_packet(self->encoder, self->packet)) >= 0) {
    callback(self->packet->data, self->packet->size,
             (self->packet->flags & AV_PKT_FLAG_KEY) ? 1 : 0, pts_us, obj);
    av_packet_unref(self->packet);
  }
  if (ret == AVERROR(EAGAIN) || ret >= 0)
    return KMS_PULL_ENCODED;
  log_av_error("encoder receive", ret);
  return self->rate_rejected ? KMS_PULL_RATE_REJECTED : -8;
}

// The encoder the pipeline opened: the VAAPI encoder it was given, or
// openh264.
const char *kms_vaapi_encoder_name(void *handle) {
  return static_cast<KmsEncoder *>(handle)->encoder_name.c_str();
}

void kms_vaapi_destroy(void *handle) {
  destroy(static_cast<KmsEncoder *>(handle));
}

} // extern "C"

// KMS/DMA-BUF capture and NVENC encode for IP-KVM OS Services, for GPUs on
// NVIDIA's proprietary driver, which has no VAAPI encoder.
//
// Each scanout dma-buf is imported with EGL on the capture GPU and drawn,
// with the hardware cursor, into an owned RGBA texture. CUDA copies that
// texture into a frame of an FFmpeg CUDA frames pool through GL interop, and
// FFmpeg's NVENC encoder encodes the frame. Desktop pixels never pass through
// CPU memory.
//
// libcuda loads at run time through the ffnvcodec loader, as FFmpeg loads it
// for NVENC, so the sender starts on a machine without the NVIDIA driver.
// The NVIDIA driver scans out through DRM KMS only under Wayland; its X
// server leaves every DRM plane empty, and the grab then finds no scanout.

#include <stdio.h>

#include <stdlib.h>

#include <algorithm>
#include <chrono>
#include <cstdint>
#include <string>
#include <vector>

#define FFNV_LOG_FUNC(logctx, msg, ...) \
  ((void)(logctx), fprintf(stderr, (msg), __VA_ARGS__))
#define FFNV_DEBUG_LOG_FUNC(logctx, msg, ...) ((void)(logctx))
#include <ffnvcodec/dynlink_loader.h>

// hwcontext_cuda.h takes the CUDA types from the loader above when it comes
// first.
extern "C" {
#include <libavcodec/avcodec.h>
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_cuda.h>
#include <libavutil/opt.h>
}

#include "kms_capture.h"

#define LOG_MODULE "KMSNVENC"
#include "log.h"
#include "cursor_gl.h"

namespace {

std::string cuda_error(CudaFunctions *cu, CUresult result) {
  const char *name = nullptr;
  if (cu->cuGetErrorName)
    cu->cuGetErrorName(result, &name);
  return name ? name : "CUresult " + std::to_string(int(result));
}

bool cuda_ok(CudaFunctions *cu, CUresult result, const char *operation) {
  if (result == CUDA_SUCCESS)
    return true;
  LOG_ERROR(std::string(operation) + " failed: " + cuda_error(cu, result));
  return false;
}

// Makes FFmpeg's CUDA context current on this thread for as long as it
// lives. The GL texture is registered with, and copied on, that context.
class CudaScope {
 public:
  CudaScope(CudaFunctions *cu, CUcontext context)
      : cu(cu), pushed(cu->cuCtxPushCurrent(context) == CUDA_SUCCESS) {}
  ~CudaScope() {
    CUcontext popped;
    if (pushed)
      cu->cuCtxPopCurrent(&popped);
  }
  CudaScope(const CudaScope &) = delete;
  CudaScope &operator=(const CudaScope &) = delete;
  explicit operator bool() const { return pushed; }

 private:
  CudaFunctions *cu;
  bool pushed;
};

// How long each stage of a frame takes. With OS_SERVICES_KMS_STAGES=1, the
// p50 and p95 of each stage go to stderr every 5 seconds. Each mark times
// the stage that ends there, from the previous mark.
class StageTimer {
 public:
  enum Stage { PACE, GRAB, CURSOR, DRAW, COPY, ENCODE, TOTAL, COUNT };

  StageTimer() {
    const char *wanted = getenv("OS_SERVICES_KMS_STAGES");
    enabled = wanted && wanted[0] == '1';
    window_started = Clock::now();
  }
  void begin_frame() {
    if (enabled)
      frame_started = last = Clock::now();
  }
  void mark(Stage stage) {
    if (!enabled)
      return;
    auto now = Clock::now();
    samples[stage].push_back(ms(now - last));
    last = now;
  }
  void end_frame() {
    if (!enabled)
      return;
    samples[TOTAL].push_back(ms(Clock::now() - frame_started));
    if (Clock::now() - window_started >= std::chrono::seconds(5))
      report();
  }

 private:
  using Clock = std::chrono::steady_clock;
  static double ms(Clock::duration d) {
    return std::chrono::duration<double, std::milli>(d).count();
  }
  void report() {
    static const char *names[COUNT] = {"pace",  "grab",   "cursor", "draw",
                                       "copy", "encode", "total"};
    double seconds = ms(Clock::now() - window_started) / 1000.0;
    fprintf(stderr, "kms nvenc stages %.1fs frames=%zu fps=%.1f", seconds,
            samples[TOTAL].size(), double(samples[TOTAL].size()) / seconds);
    for (int stage = 0; stage < COUNT; ++stage) {
      auto &values = samples[stage];
      std::sort(values.begin(), values.end());
      auto at = [&](double q) {
        return values.empty()
                   ? 0.0
                   : values[std::min(values.size() - 1,
                                     size_t(q * double(values.size())))];
      };
      fprintf(stderr, " %s=%.2f/%.2f", names[stage], at(0.5), at(0.95));
      values.clear();
    }
    fprintf(stderr, " (ms p50/p95)\n");
    window_started = Clock::now();
  }

  bool enabled = false;
  Clock::time_point window_started, frame_started, last;
  std::vector<double> samples[COUNT];
};

struct NvencPipeline {
  KmsCapture capture;
  CursorGL gl;
  // The owned texture each scanout is drawn into, with the cursor on top.
  GLuint target = 0;
  CudaFunctions *cu = nullptr;
  CUgraphicsResource resource = nullptr;
  AVBufferRef *device_ref = nullptr;
  AVBufferRef *frames_ref = nullptr;
  CUcontext cuda_context = nullptr;
  AVCodecContext *encoder = nullptr;
  AVPacket *packet = nullptr;
  AVFrame *frame = nullptr;
  int kbs = 0;
  int fps = 0;
  int gop = 0;
  std::string encoder_name;
  KmsCursor hw_cursor;
  // The last frame went without the cursor; the log says why once.
  bool cursor_missing = false;
  StageTimer stages;
};

void destroy(NvencPipeline *self) {
  if (!self)
    return;
  if (self->resource) {
    self->gl.make_current();
    CudaScope scope(self->cu, self->cuda_context);
    self->cu->cuGraphicsUnregisterResource(self->resource);
  }
  avcodec_free_context(&self->encoder);
  av_packet_free(&self->packet);
  av_frame_free(&self->frame);
  av_buffer_unref(&self->frames_ref);
  av_buffer_unref(&self->device_ref);
  if (self->cu)
    cuda_free_functions(&self->cu);
  if (self->target && self->gl.make_current())
    glDeleteTextures(1, &self->target);
  self->capture.close();
  delete self;
}

// The RGBA texture the scanout is drawn into. CUDA registers only textures
// with a sized internal format, which needs a GLES 3 context.
bool make_target(NvencPipeline *self) {
  glGenTextures(1, &self->target);
  glBindTexture(GL_TEXTURE_2D, self->target);
  glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
  glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
  glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8_OES, self->capture.width,
               self->capture.height, 0, GL_RGBA, GL_UNSIGNED_BYTE, nullptr);
  GLenum error = glGetError();
  if (error == GL_NO_ERROR)
    return true;
  LOG_ERROR("owned RGBA texture failed: GL error " + std::to_string(error));
  return false;
}

// FFmpeg's CUDA device on the GPU behind the GL context, a pool of RGB0
// frames of the scanout's size, and the texture registered for copies.
KmsOpenFailure build_cuda(NvencPipeline *self) {
  if (cuda_load_functions(&self->cu, nullptr) < 0) {
    LOG_ERROR("libcuda.so.1 is not loadable; the NVIDIA driver is missing");
    return KMS_ENCODER_UNAVAILABLE;
  }
  if (!cuda_ok(self->cu, self->cu->cuInit(0), "cuInit"))
    return KMS_ENCODER_UNAVAILABLE;
  unsigned int count = 0;
  CUdevice devices[4] = {};
  if (!cuda_ok(self->cu,
               self->cu->cuGLGetDevices(&count, devices, 4,
                                        CU_GL_DEVICE_LIST_ALL),
               "cuGLGetDevices"))
    return KMS_ENCODER_UNAVAILABLE;
  if (count == 0) {
    LOG_ERROR("no CUDA device behind the capture GPU's GL context");
    return KMS_ENCODER_UNAVAILABLE;
  }
  std::string ordinal = std::to_string(int(devices[0]));
  int ret = av_hwdevice_ctx_create(&self->device_ref, AV_HWDEVICE_TYPE_CUDA,
                                   ordinal.c_str(), nullptr, 0);
  if (ret < 0) {
    log_av_error("CUDA device open", ret);
    return KMS_ENCODER_UNAVAILABLE;
  }
  auto device = reinterpret_cast<AVHWDeviceContext *>(self->device_ref->data);
  self->cuda_context =
      static_cast<AVCUDADeviceContext *>(device->hwctx)->cuda_ctx;

  self->frames_ref = av_hwframe_ctx_alloc(self->device_ref);
  if (!self->frames_ref)
    return KMS_PIPELINE_FAILED;
  auto frames = reinterpret_cast<AVHWFramesContext *>(self->frames_ref->data);
  frames->format = AV_PIX_FMT_CUDA;
  // The texture holds R, G, B, A bytes, which NVENC reads as RGB0.
  frames->sw_format = AV_PIX_FMT_RGB0;
  frames->width = self->capture.width;
  frames->height = self->capture.height;
  if ((ret = av_hwframe_ctx_init(self->frames_ref)) < 0) {
    log_av_error("CUDA frames init", ret);
    return KMS_PIPELINE_FAILED;
  }

  CudaScope scope(self->cu, self->cuda_context);
  if (!scope ||
      !cuda_ok(self->cu,
               self->cu->cuGraphicsGLRegisterImage(
                   &self->resource, self->target, GL_TEXTURE_2D,
                   CU_GRAPHICS_REGISTER_FLAGS_READ_ONLY),
               "cuGraphicsGLRegisterImage"))
    return KMS_PIPELINE_FAILED;
  return KMS_OPENED;
}

KmsOpenFailure build_encoder(NvencPipeline *self) {
  const AVCodec *codec =
      avcodec_find_encoder_by_name(self->encoder_name.c_str());
  if (!codec) {
    LOG_ERROR(self->encoder_name + " encoder not available");
    return KMS_ENCODER_UNAVAILABLE;
  }
  self->encoder = avcodec_alloc_context3(codec);
  self->encoder->width = self->capture.width;
  self->encoder->height = self->capture.height;
  self->encoder->pix_fmt = AV_PIX_FMT_CUDA;
  self->encoder->sw_pix_fmt = AV_PIX_FMT_RGB0;
  self->encoder->time_base = av_make_q(1, 1000000);
  self->encoder->framerate = av_make_q(self->fps, 1);
  // VBR with a generous peak, as the VAAPI encoder: the average targets kbs,
  // and bursts may use up to 2.5x during scrolling and motion.
  self->encoder->bit_rate = int64_t(self->kbs) * 1000;
  self->encoder->rc_max_rate = peak_bit_rate(self->kbs);
  self->encoder->rc_buffer_size = int(peak_bit_rate(self->kbs));
  self->encoder->gop_size = self->gop;
  self->encoder->max_b_frames = 0;
  self->encoder->color_range = AVCOL_RANGE_MPEG;
  self->encoder->colorspace = AVCOL_SPC_SMPTE170M;
  self->encoder->color_primaries = AVCOL_PRI_SMPTE170M;
  self->encoder->color_trc = AVCOL_TRC_SMPTE170M;
  self->encoder->hw_frames_ctx = av_buffer_ref(self->frames_ref);
  // The fastest preset with ultra-low-latency tuning, and every picture out
  // of the encoder before the next goes in. A forced keyframe is an IDR, so
  // a viewer can start decoding at it.
  const char *options[][2] = {
      {"preset", "p1"}, {"tune", "ull"}, {"zerolatency", "1"},
      {"delay", "0"},   {"rc", "vbr"},   {"forced-idr", "1"},
  };
  for (auto &option : options) {
    int ret = av_opt_set(self->encoder->priv_data, option[0], option[1], 0);
    if (ret < 0) {
      log_av_error((self->encoder_name + " option " + option[0]).c_str(), ret);
      return KMS_ENCODER_UNAVAILABLE;
    }
  }
  int ret = avcodec_open2(self->encoder, codec, nullptr);
  if (ret < 0) {
    log_av_error((self->encoder_name + " open").c_str(), ret);
    return KMS_ENCODER_UNAVAILABLE;
  }
  return KMS_OPENED;
}

// Copies the drawn texture into `frame`. Mapping the texture orders the copy
// after the GL work on it.
bool copy_target(NvencPipeline *self, AVFrame *frame) {
  CudaScope scope(self->cu, self->cuda_context);
  if (!scope) {
    LOG_ERROR("CUDA context push failed");
    return false;
  }
  CudaFunctions *cu = self->cu;
  if (!cuda_ok(cu, cu->cuGraphicsMapResources(1, &self->resource, nullptr),
               "cuGraphicsMapResources"))
    return false;
  CUarray array = nullptr;
  bool copied = cuda_ok(cu,
                        cu->cuGraphicsSubResourceGetMappedArray(
                            &array, self->resource, 0, 0),
                        "cuGraphicsSubResourceGetMappedArray");
  if (copied) {
    CUDA_MEMCPY2D copy = {};
    copy.srcMemoryType = CU_MEMORYTYPE_ARRAY;
    copy.srcArray = array;
    copy.dstMemoryType = CU_MEMORYTYPE_DEVICE;
    copy.dstDevice = CUdeviceptr(frame->data[0]);
    copy.dstPitch = size_t(frame->linesize[0]);
    copy.WidthInBytes = size_t(self->capture.width) * 4;
    copy.Height = size_t(self->capture.height);
    copied = cuda_ok(cu, cu->cuMemcpy2D(&copy), "cuMemcpy2D");
  }
  bool unmapped =
      cuda_ok(cu, cu->cuGraphicsUnmapResources(1, &self->resource, nullptr),
              "cuGraphicsUnmapResources");
  return copied && unmapped;
}

} // namespace

extern "C" {

// Opens a capture on `device` and builds the whole pipeline behind it. On
// failure returns nullptr and stores why in `failure` (a KmsOpenFailure).
void *kms_nvenc_new_encoder(const char *device, uint32_t connector_id,
                            const char *encoder_name, int32_t framerate,
                            int32_t kbs, int32_t gop, int32_t *failure) {
  auto self = new NvencPipeline();
  self->kbs = kbs;
  self->fps = framerate;
  self->gop = gop;
  self->encoder_name =
      (encoder_name && encoder_name[0]) ? encoder_name : "h264_nvenc";

  KmsFrame first;
  *failure = self->capture.open(device, connector_id, first);
  if (*failure != KMS_OPENED) {
    destroy(self);
    return nullptr;
  }
  self->hw_cursor.open(device, self->capture.crtc_id);
  // The scanout's geometry, format and modifier fix the whole pipeline, so
  // everything is built here rather than off the first streamed frame: an
  // import or encoder this machine cannot do should fail the open, not frame
  // one. Drawing the first frame proves that EGL imports the scanout.
  const char *node = self->capture.lib->render_node(self->capture.ctx);
  if (!self->capture.adopt(first.desc)) {
    *failure = KMS_PIPELINE_FAILED;
  } else if (!self->gl.init(node, 3) || !make_target(self)) {
    LOG_ERROR(std::string("EGL on ") + (node ? node : "no render node") +
              " failed");
    *failure = KMS_PIPELINE_FAILED;
  } else {
    switch (self->gl.draw_scanout(first.desc, self->target,
                                  self->capture.width, self->capture.height,
                                  drmtap_cursor_info{})) {
    case CursorGL::Drawn::Ok:
      break;
    case CursorGL::Drawn::ImportRefused:
      LOG_ERROR("EGL refused the scanout's format or modifier");
      *failure = KMS_FORMAT_UNSUPPORTED;
      break;
    case CursorGL::Drawn::Failed:
      LOG_ERROR("drawing the scanout failed");
      *failure = KMS_PIPELINE_FAILED;
      break;
    }
  }
  self->capture.release(first);
  if (*failure == KMS_OPENED)
    *failure = build_cuda(self);
  if (*failure == KMS_OPENED)
    *failure = build_encoder(self);
  if (*failure != KMS_OPENED) {
    destroy(self);
    return nullptr;
  }
  self->packet = av_packet_alloc();
  self->frame = av_frame_alloc();
  self->capture.start_pacing(self->fps);
  return self;
}

int32_t kms_nvenc_dimensions(void *handle, int32_t *width, int32_t *height) {
  auto self = static_cast<NvencPipeline *>(handle);
  *width = self->capture.width;
  *height = self->capture.height;
  return 0;
}

// A new bitrate for the running encoder. Before each frame, FFmpeg's NVENC
// encoder compares bit_rate, rc_max_rate and rc_buffer_size with its session
// and reconfigures the session in place when they differ. The FFmpeg port's
// ffmpeg-nvenc-bitrate.patch removes the encoder reset and the IDR from that
// reconfiguration, so the stream goes on at the new rate with no keyframe.
// The frame rate the session budgets bits by is fixed at open, so a new
// frame rate returns -1 and the caller builds a new encoder.
int32_t kms_nvenc_set_rate(void *handle, int32_t framerate, int32_t kbs) {
  auto self = static_cast<NvencPipeline *>(handle);
  if (kbs <= 0 || framerate != self->fps)
    return -1;
  self->kbs = kbs;
  self->encoder->bit_rate = int64_t(kbs) * 1000;
  self->encoder->rc_max_rate = peak_bit_rate(kbs);
  self->encoder->rc_buffer_size = int(peak_bit_rate(kbs));
  return 0;
}

int32_t kms_nvenc_next(void *handle, KmsPacketCallback callback,
                       const void *obj, int32_t force_idr) {
  auto self = static_cast<NvencPipeline *>(handle);
  self->stages.begin_frame();
  self->capture.wait_for_frame();
  self->stages.mark(StageTimer::PACE);
  KmsFrame grabbed;
  int32_t pull = self->capture.grab(grabbed);
  if (pull != KMS_PULL_ENCODED)
    return pull;
  self->stages.mark(StageTimer::GRAB);
  const bool other_modifier = self->capture.other_modifier();
  const uint64_t pts_us =
      std::chrono::duration_cast<std::chrono::microseconds>(
          std::chrono::steady_clock::now().time_since_epoch())
          .count();

  // A cursor that cannot be read or drawn leaves the frame without it, and
  // the stream goes on.
  drmtap_cursor_info cursor = {};
  int ret = self->hw_cursor.read(self->capture, cursor);
  self->stages.mark(StageTimer::CURSOR);
  const bool oversize = ret >= 0 && cursor.visible &&
                        (cursor.width > 1024 || cursor.height > 1024);
  const bool drawable = ret >= 0 && !oversize;
  if (!drawable && !self->cursor_missing) {
    LOG_ERROR((oversize ? std::string("cursor larger than 1024x1024 not drawn")
                        : "cursor query failed: " + std::to_string(ret)));
  }
  self->cursor_missing = !drawable;
  CursorGL::Drawn drawn = self->gl.draw_scanout(
      grabbed.desc, self->target, self->capture.width, self->capture.height,
      drawable ? cursor : drmtap_cursor_info{});
  self->capture.release(grabbed);
  self->stages.mark(StageTimer::DRAW);
  if (drawn != CursorGL::Drawn::Ok) {
    if (drawn == CursorGL::Drawn::ImportRefused)
      LOG_ERROR("EGL refused the scanout");
    return other_modifier ? KMS_PULL_SCANOUT_CHANGED : -6;
  }

  ret = av_hwframe_get_buffer(self->frames_ref, self->frame, 0);
  if (ret < 0) {
    log_av_error("CUDA frame", ret);
    return -3;
  }
  if (!copy_target(self, self->frame)) {
    av_frame_unref(self->frame);
    return -4;
  }
  self->stages.mark(StageTimer::COPY);
  self->frame->pts = int64_t(pts_us);
  self->frame->pict_type = force_idr ? AV_PICTURE_TYPE_I : AV_PICTURE_TYPE_NONE;
  ret = avcodec_send_frame(self->encoder, self->frame);
  av_frame_unref(self->frame);
  if (ret < 0) {
    log_av_error("encoder submit", ret);
    return -7;
  }
  while ((ret = avcodec_receive_packet(self->encoder, self->packet)) >= 0) {
    callback(self->packet->data, self->packet->size,
             (self->packet->flags & AV_PKT_FLAG_KEY) ? 1 : 0, pts_us, obj);
    av_packet_unref(self->packet);
  }
  self->stages.mark(StageTimer::ENCODE);
  self->stages.end_frame();
  if (ret == AVERROR(EAGAIN) || ret >= 0)
    return KMS_PULL_ENCODED;
  log_av_error("encoder receive", ret);
  return -8;
}

// The encoder the pipeline opened.
const char *kms_nvenc_encoder_name(void *handle) {
  return static_cast<NvencPipeline *>(handle)->encoder_name.c_str();
}

void kms_nvenc_destroy(void *handle) {
  destroy(static_cast<NvencPipeline *>(handle));
}

} // extern "C"

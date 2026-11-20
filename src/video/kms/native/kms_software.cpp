// The CPU path of the KMS capture, for a machine where no GPU pipeline
// builds: the VAAPI driver does not load, has no video processing, or the
// GPU's encoder will not open. libdrmtap brings the scanout to memory as
// linear pixels, detiling it with EGL where the layout needs it; the CPU
// converts it to I420 with the cursor drawn in (cpu_frame.h), and OpenH264
// encodes it. This costs CPU time and frame rate, so the capture tries it
// only after the GPU pipelines (KmsEncoder::new in capture.rs).
//
// An unchanged screen costs a grab and a compare, and encodes nothing. Once
// the screen has been unchanged for IDLE_AFTER, the grabs slow to one each
// IDLE_PERIOD: a grab detiles the whole scanout, which costs a quarter of a
// core at 60 grabs a second on an Intel N100 at 2560x1440.

#include <algorithm>
#include <chrono>
#include <cstdint>
#include <memory>
#include <string>

extern "C" {
#include <drm_fourcc.h>
}

#include "kms_capture.h"

#define LOG_MODULE "KMSCPU"
#include "log.h"
#include "cpu_frame.h"
#include "openh264.h"

namespace {

// The name the capture reports for this path, distinct from the openh264
// of the VAAPI pipeline, so that the log shows which path runs.
constexpr const char *ENCODER_NAME = "openh264_cpu";
constexpr std::chrono::seconds IDLE_AFTER{1};
// The longest a change waits on an idle screen before it is grabbed.
constexpr std::chrono::milliseconds IDLE_PERIOD{100};

struct SoftwareEncoder {
  KmsCapture capture;
  KmsCursor cursor;
  CpuFrame frame;
  std::unique_ptr<Openh264> encoder;
  int fps = 0;
  int kbs = 0;
  // When the picture or the cursor last changed.
  std::chrono::steady_clock::time_point changed_at;
};

// Whether the CPU path reads pixels of `fourcc`, and in which byte order:
// libdrmtap gives 8-bit pixels, in the scanout's channel order where it
// does not convert them.
bool byte_order(uint32_t fourcc, bool &red_first) {
  switch (fourcc) {
  case DRM_FORMAT_XRGB8888:
  case DRM_FORMAT_ARGB8888:
    red_first = false;
    return true;
  case DRM_FORMAT_XBGR8888:
  case DRM_FORMAT_ABGR8888:
    red_first = true;
    return true;
  default:
    return false;
  }
}

CpuPointer pointer_of(const drmtap_cursor_info &cursor) {
  CpuPointer pointer;
  // A larger image is not a cursor; the VAAPI path refuses it as well.
  if (!cursor.visible || !cursor.pixels || cursor.width > 1024 || cursor.height > 1024)
    return pointer;
  pointer.visible = true;
  pointer.x = cursor.x;
  pointer.y = cursor.y;
  pointer.width = int(cursor.width);
  pointer.height = int(cursor.height);
  pointer.pixels = cursor.pixels;
  return pointer;
}

} // namespace

extern "C" {

// Opens a capture on `device` and the CPU encode behind it. `encoder_name`
// is not used: this path has one encoder. On failure returns nullptr and
// stores why in `failure` (a KmsOpenFailure).
void *kms_software_new_encoder(const char *device, uint32_t connector_id,
                               const char *encoder_name, int32_t framerate, int32_t kbs,
                               int32_t gop, int32_t *failure) {
  (void)encoder_name;
  // OpenH264 adds no IDR of its own; the KVM asks for one when it needs it.
  (void)gop;
  auto self = std::make_unique<SoftwareEncoder>();
  self->capture.mapped = true;
  KmsFrame first;
  *failure = self->capture.open(device, connector_id, first);
  if (*failure != KMS_OPENED)
    return nullptr;
  bool red_first = false;
  if (!byte_order(first.desc.format, red_first)) {
    LOG_ERROR("mapped scanout format " + std::to_string(first.desc.format) +
              " is not 8-bit RGB");
    self->capture.release(first);
    *failure = KMS_FORMAT_UNSUPPORTED;
    return nullptr;
  }
  self->capture.adopt(first.desc);
  self->capture.release(first);

  self->fps = std::min(int(framerate), Openh264::kMaxFps);
  self->kbs = kbs;
  self->encoder = std::make_unique<Openh264>();
  if (!self->encoder->open(self->capture.width, self->capture.height, self->fps, kbs)) {
    *failure = KMS_ENCODER_UNAVAILABLE;
    return nullptr;
  }
  self->frame.reset(self->capture.width, self->capture.height, red_first);
  self->cursor.open(device, self->capture.crtc_id);
  self->capture.start_pacing(self->fps);
  return self.release();
}

int32_t kms_software_dimensions(void *handle, int32_t *width, int32_t *height) {
  auto self = static_cast<SoftwareEncoder *>(handle);
  *width = self->capture.width;
  *height = self->capture.height;
  return 0;
}

// OpenH264 takes a new rate in place, and the pacing changes at once.
int32_t kms_software_set_rate(void *handle, int32_t framerate, int32_t kbs) {
  auto self = static_cast<SoftwareEncoder *>(handle);
  if (framerate <= 0 || kbs <= 0)
    return -1;
  self->kbs = kbs;
  self->fps = std::min(int(framerate), Openh264::kMaxFps);
  self->capture.set_pacing(self->fps);
  self->encoder->set_rate(self->fps, kbs);
  return 0;
}

int32_t kms_software_next(void *handle, KmsPacketCallback callback, const void *obj,
                          int32_t force_idr) {
  auto self = static_cast<SoftwareEncoder *>(handle);
  self->capture.wait_for_frame();
  KmsFrame grabbed;
  int32_t pull = self->capture.grab(grabbed);
  if (pull != KMS_PULL_ENCODED)
    return pull;
  const uint64_t pts_us = uint64_t(std::chrono::duration_cast<std::chrono::microseconds>(
                                       std::chrono::steady_clock::now().time_since_epoch())
                                       .count());
  // A cursor that cannot be read is left out of the frame rather than
  // ending the video.
  drmtap_cursor_info cursor = {};
  if (self->cursor.read(self->capture, cursor) < 0)
    cursor = {};
  const CpuFrame::Change change =
      self->frame.update(static_cast<const uint8_t *>(grabbed.info.data), grabbed.info.stride,
                         pointer_of(cursor));
  self->capture.release(grabbed);
  const auto now = std::chrono::steady_clock::now();
  if (change != CpuFrame::UNCHANGED)
    self->changed_at = now;
  else if (now - self->changed_at >= IDLE_AFTER)
    self->capture.next_frame = std::max(self->capture.next_frame, now + IDLE_PERIOD);
  // A keyframe request on an unchanged screen encodes the frame it has.
  if (change == CpuFrame::UNCHANGED && !force_idr)
    return KMS_PULL_UNCHANGED;

  const CpuFrame &frame = self->frame;
  int ret = self->encoder->encode(frame.y(), frame.width(), frame.u(), frame.v(),
                                  frame.chroma_width(), force_idr != 0, pts_us,
                                  [&](const uint8_t *data, int size, bool key) {
                                    callback(data, size, key ? 1 : 0, pts_us, obj);
                                  });
  if (ret != 0) {
    LOG_ERROR("OpenH264 encode failed: " + std::to_string(ret));
    return -8;
  }
  return change == CpuFrame::POINTER ? KMS_PULL_POINTER : KMS_PULL_ENCODED;
}

const char *kms_software_encoder_name(void *) { return ENCODER_NAME; }

void kms_software_destroy(void *handle) { delete static_cast<SoftwareEncoder *>(handle); }

// The CPU frame alone, for the tests in src/video/kms/cpu_frame.rs: a
// width x height frame of 8-bit pixels, updated with a picture and a
// cursor of premultiplied ARGB8888 pixels at (x, y). The update returns a
// CpuFrame::Change; the planes stay valid until the next call.
void *kms_cpu_frame_new(int32_t width, int32_t height, int32_t red_first) {
  auto frame = new CpuFrame();
  frame->reset(width, height, red_first != 0);
  return frame;
}

int32_t kms_cpu_frame_update(void *handle, const uint8_t *pixels, uint32_t stride,
                             const uint32_t *cursor, int32_t x, int32_t y, int32_t width,
                             int32_t height) {
  CpuPointer pointer;
  pointer.visible = cursor != nullptr;
  pointer.x = x;
  pointer.y = y;
  pointer.width = width;
  pointer.height = height;
  pointer.pixels = cursor;
  return static_cast<CpuFrame *>(handle)->update(pixels, stride, pointer);
}

void kms_cpu_frame_planes(void *handle, const uint8_t **y, const uint8_t **u,
                          const uint8_t **v) {
  auto frame = static_cast<CpuFrame *>(handle);
  *y = frame->y();
  *u = frame->u();
  *v = frame->v();
}

void kms_cpu_frame_destroy(void *handle) { delete static_cast<CpuFrame *>(handle); }

} // extern "C"

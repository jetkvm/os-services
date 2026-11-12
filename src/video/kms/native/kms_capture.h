// The scanout capture that both KMS encoders share: libdrmtap loading, the
// open and first grab of a display, the pacing of grabs, and the detection
// of a changed display configuration or tiling modifier.
//
// The privilege lives outside this process: libdrmtap spawns its
// drmtap-helper, which carries CAP_SYS_ADMIN, reads the framebuffer handle
// and hands the dma-buf over SCM_RIGHTS. This process needs only a render
// node, so the sender runs as an ordinary user. libdrmtap is loaded at run
// time so the sender starts, and says what is missing, on a machine without
// it.

#pragma once

#include <stddef.h>
#include <stdint.h>

#include <chrono>
#include <vector>

#include "drmtap.h"

// Why an open failed, reported through the encoders' new_encoder functions
// so the caller can act on the cause rather than guess it: only a missing
// scanout is worth waiting for, only a missing encoder is worth a codec
// fallback, and only a missing helper is something an install fixes. The
// Rust side mirrors these values (KmsFailure in src/video/kms/capture.rs).
enum KmsOpenFailure : int32_t {
  KMS_OPENED = 0,
  // libdrmtap is not loadable, or not the version this was built against.
  KMS_LIBRARY_UNAVAILABLE = 1,
  // The DRM device would not open.
  KMS_DEVICE_UNAVAILABLE = 2,
  // drmtap-helper is missing, or this account may not run it.
  KMS_HELPER_UNAVAILABLE = 3,
  // No live framebuffer of a desktop: the display is off, re-plugging, or
  // between two modes.
  KMS_NO_SCANOUT = 4,
  // The encoder's GPU API cannot import the scanout format.
  KMS_FORMAT_UNSUPPORTED = 5,
  // The named encoder would not open.
  KMS_ENCODER_UNAVAILABLE = 6,
  // The grab, hardware context or filter graph failed.
  KMS_PIPELINE_FAILED = 7,
  // This account may not open the GPU's render node.
  KMS_RENDER_NODE_DENIED = 8,
};

// What one pull of a pipeline did, as the encoders' next functions report
// it. Negative values are failures.
enum KmsPull : int32_t {
  KMS_PULL_ENCODED = 0,
  KMS_PULL_IDLE = 1,
  // The scanout has another size or layout; rebuild the pipeline for it.
  KMS_PULL_SCANOUT_CHANGED = 2,
  // The driver refused a new rate for the running encoder; build a new
  // encoder at that rate.
  KMS_PULL_RATE_REJECTED = 3,
  KMS_PULL_GRAB_FAILED = -1,
};

typedef void (*KmsPacketCallback)(const uint8_t *data, int32_t len,
                                  int32_t key, uint64_t pts_us,
                                  const void *obj);

void log_av_error(const char *operation, int error);

// The peak rate of the VBR encoders: bursts may use up to 2.5 times the
// average during scrolling and motion.
inline int64_t peak_bit_rate(int kbs) { return int64_t(kbs) * 1000 * 5 / 2; }

// libdrmtap entry points, resolved once from libdrmtap.so.0.
struct Drmtap {
  void *handle = nullptr;
  drmtap_ctx *(*open)(const drmtap_config *) = nullptr;
  void (*close)(drmtap_ctx *) = nullptr;
  int (*list_displays)(drmtap_ctx *, drmtap_display *, int) = nullptr;
  int (*displays_changed)(drmtap_ctx *) = nullptr;
  int (*grab_desc)(drmtap_ctx *, drmtap_dmabuf_desc *, drmtap_frame_info *) =
      nullptr;
  void (*frame_release)(drmtap_ctx *, drmtap_frame_info *) = nullptr;
  int (*get_cursor)(drmtap_ctx *, drmtap_cursor_info *) = nullptr;
  void (*cursor_release)(drmtap_ctx *, drmtap_cursor_info *) = nullptr;
  const char *(*error)(drmtap_ctx *) = nullptr;
  const char *(*render_node)(drmtap_ctx *) = nullptr;
};

// The loaded library, or nullptr when it is missing or of another version.
const Drmtap *drmtap();

void log_drmtap_error(const Drmtap *lib, drmtap_ctx *ctx, const char *what);

// The shortest side, in pixels, of a scanout that counts as a desktop. KWin
// has a 32x32 framebuffer on the primary plane while it sets a mode, and the
// desktop's framebuffer follows it.
constexpr uint32_t MIN_SCANOUT_SIDE = 128;

// One grabbed scanout, held from the grab until KmsCapture::release.
struct KmsFrame {
  drmtap_dmabuf_desc desc = {};
  drmtap_frame_info info = {};
};

// A capture of one display. Its geometry, format and modifier are those of
// the scanout the pipeline was built for; a grab of another size or format
// is a new display configuration.
struct KmsCapture {
  const Drmtap *lib = nullptr;
  drmtap_ctx *ctx = nullptr;
  // The CRTC behind the connector the capture was opened for; 0 when
  // libdrmtap chose the display.
  uint32_t crtc_id = 0;
  int width = 0;
  int height = 0;
  uint32_t fourcc = 0;
  uint64_t modifier = 0;
  // The modifier the pipeline was built with. While frames carry another, a
  // failed import rebuilds the pipeline.
  uint64_t opened_modifier = 0;
  size_t buffer_size = 0;
  std::chrono::steady_clock::time_point next_frame;
  std::chrono::microseconds period{0};

  KmsCapture() = default;
  KmsCapture(const KmsCapture &) = delete;
  KmsCapture &operator=(const KmsCapture &) = delete;
  ~KmsCapture() { close(); }

  // Opens a capture on `device`, of the CRTC behind `connector_id` when it
  // is not 0, and grabs the first frame into `first`. The first grab must
  // succeed, with a frame of MIN_SCANOUT_SIDE or more a side, for the display
  // to count as capturable; a smaller frame is KMS_NO_SCANOUT. The first grab
  // is also where libdrmtap first runs the helper, so a helper this account
  // cannot use surfaces here rather than at open.
  KmsOpenFailure open(const char *device, uint32_t connector_id,
                      KmsFrame &first);
  void close();
  // Takes the geometry, format and modifier of `desc` as the pipeline's.
  // False when the dma-buf size cannot be read.
  bool adopt(const drmtap_dmabuf_desc &desc);
  // Grabs are paced at `fps`, starting now.
  void start_pacing(int fps);
  // A new rate takes effect at once: a frame due after one period of the
  // new rate comes at that period.
  void set_pacing(int fps);
  // Sleeps until the next frame is due. Advancing before the work keeps the
  // period start-to-start; the clamp stops a burst catching up after an
  // overrun.
  void wait_for_frame();
  // Grabs the current scanout. Returns KMS_PULL_ENCODED with `frame` held,
  // or, with nothing held, the pull to report: KMS_PULL_SCANOUT_CHANGED for
  // a new display configuration and KMS_PULL_GRAB_FAILED for a failed grab.
  int32_t grab(KmsFrame &frame);
  void release(KmsFrame &frame);
  // The last grab carries another modifier than the pipeline was built
  // with.
  bool other_modifier() const { return modifier != opened_modifier; }
};

// The hardware cursor of a capture, read cheaply. libdrmtap reads the cursor
// through its helper, which copies the whole image from the cursor buffer on
// each call; on a GPU whose cursor buffer is in video memory that costs
// milliseconds. The cursor plane's properties need no privilege, so they give
// the framebuffer and position on every read, and the image comes through
// libdrmtap when the framebuffer changes, when the cursor shows again after
// it was hidden, and at least every CURSOR_REFRESH while it shows. A
// framebuffer ID names a buffer, not its contents: a compositor can draw a
// new cursor image into the buffer it shows, and only the refresh sees that.
class KmsCursor {
 public:
  // The longest a cursor image that changed in place shows stale. A refresh
  // costs one helper read: 8 ms on an RTX 3060, so about 3% of the frame
  // time at this interval.
  static constexpr std::chrono::milliseconds CURSOR_REFRESH{250};

  KmsCursor() = default;
  KmsCursor(const KmsCursor &) = delete;
  KmsCursor &operator=(const KmsCursor &) = delete;
  ~KmsCursor();

  // Opens `device` for the plane queries, for the cursor of `crtc_id` (0:
  // the first CRTC with a cursor). Without the device, or on a driver
  // without atomic plane properties, every read goes through libdrmtap.
  void open(const char *device, uint32_t crtc_id);
  // The cursor as the scanout shows it now. `out.pixels` points into this
  // object and stays valid until the next read; `out` is not for
  // drmtap_cursor_release.
  int read(const KmsCapture &capture, drmtap_cursor_info &out);

 private:
  bool bound(uint64_t plane_crtc) const;
  bool load_planes();
  // The framebuffer, CRTC and position of `plane`.
  bool plane_state(uint32_t plane, uint64_t &fb, uint64_t &plane_crtc,
                   int32_t &x, int32_t &y);
  int fetch_image(const KmsCapture &capture);

  int fd = -1;
  uint32_t crtc = 0;
  // Every cursor plane of the device; the one last bound to the CRTC first.
  std::vector<uint32_t> planes;
  uint32_t prop_fb = 0, prop_crtc = 0, prop_x = 0, prop_y = 0;
  // The framebuffer the kept image came from, and when; 0 when none is kept.
  uint64_t image_fb = 0;
  std::chrono::steady_clock::time_point image_at;
  drmtap_cursor_info image = {};
  std::vector<uint32_t> pixels;
};

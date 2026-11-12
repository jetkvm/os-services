#include "kms_capture.h"

#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <stdlib.h>
#include <unistd.h>

#include <algorithm>
#include <string>
#include <thread>
#include <utility>

extern "C" {
#include <libavutil/error.h>
#include <xf86drm.h>
#include <xf86drmMode.h>
}

#define LOG_MODULE "KMS"
#include "log.h"

void log_av_error(const char *operation, int error) {
  char message[AV_ERROR_MAX_STRING_SIZE] = {};
  av_make_error_string(message, sizeof(message), error);
  LOG_ERROR(std::string(operation) + " failed: " + message + " (" +
            std::to_string(error) + ")");
}

namespace {

Drmtap load_drmtap() {
  Drmtap lib;
  // A packaged sender carries its own copy and names it in the environment;
  // otherwise the loader's search path finds a system install.
  const char *bundled = getenv("OS_SERVICES_DRMTAP_LIB");
  if (bundled && bundled[0])
    lib.handle = dlopen(bundled, RTLD_NOW | RTLD_LOCAL);
  if (!lib.handle)
    lib.handle = dlopen("libdrmtap.so.0", RTLD_NOW | RTLD_LOCAL);
  if (!lib.handle) {
    LOG_ERROR(std::string("libdrmtap.so.0 not loadable: ") + dlerror());
    return lib;
  }
  bool complete = true;
  auto resolve = [&](const char *name) {
    void *symbol = dlsym(lib.handle, name);
    if (!symbol) {
      LOG_ERROR(std::string("libdrmtap.so.0 lacks ") + name);
      complete = false;
    }
    return symbol;
  };
  auto version = (int (*)())resolve("drmtap_version");
  lib.open = (decltype(lib.open))resolve("drmtap_open");
  lib.close = (decltype(lib.close))resolve("drmtap_close");
  lib.list_displays = (decltype(lib.list_displays))resolve("drmtap_list_displays");
  lib.displays_changed = (decltype(lib.displays_changed))resolve("drmtap_displays_changed");
  lib.grab_desc = (decltype(lib.grab_desc))resolve("drmtap_grab_desc");
  lib.frame_release =
      (decltype(lib.frame_release))resolve("drmtap_frame_release");
  lib.get_cursor = (decltype(lib.get_cursor))resolve("drmtap_get_cursor");
  lib.cursor_release = (decltype(lib.cursor_release))resolve("drmtap_cursor_release");
  lib.error = (decltype(lib.error))resolve("drmtap_error");
  lib.render_node = (decltype(lib.render_node))resolve("drmtap_render_node");
  // The struct layouts come from the vendored header, so a library built
  // from another minor version would be read with the wrong offsets.
  if (complete) {
    int found = version();
    int wanted = (DRMTAP_VERSION_MAJOR << 16) | (DRMTAP_VERSION_MINOR << 8);
    if ((found & ~0xFF) != wanted) {
      LOG_ERROR("libdrmtap " + std::to_string((found >> 16) & 0xFF) + "." +
                std::to_string((found >> 8) & 0xFF) + " does not match the " +
                std::to_string(DRMTAP_VERSION_MAJOR) + "." +
                std::to_string(DRMTAP_VERSION_MINOR) + " this was built for");
      complete = false;
    }
  }
  if (!complete) {
    dlclose(lib.handle);
    lib.handle = nullptr;
  }
  return lib;
}

} // namespace

const Drmtap *drmtap() {
  static Drmtap lib = load_drmtap();
  return lib.handle ? &lib : nullptr;
}

void log_drmtap_error(const Drmtap *lib, drmtap_ctx *ctx, const char *what) {
  const char *why = lib->error(ctx);
  LOG_ERROR(std::string(what) + ": " + (why ? why : "unknown"));
}

KmsOpenFailure KmsCapture::open(const char *device, uint32_t connector_id,
                                KmsFrame &first) {
  lib = drmtap();
  if (!lib)
    return KMS_LIBRARY_UNAVAILABLE;

  drmtap_config config = {};
  config.device_path = (device && device[0]) ? device : nullptr;
  // Name the helper rather than relying on the library's search order, which
  // looks beside the executable and in /usr/libexec before the privileged
  // copy the packages install. Anything else found there would lack the
  // capability, and capture would fail after a successful install.
  const char *helper = getenv("OS_SERVICES_DRMTAP_HELPER");
  if (helper && helper[0])
    config.helper_path = helper;
  ctx = lib->open(&config);
  if (!ctx) {
    log_drmtap_error(lib, nullptr, "drmtap open failed");
    return KMS_DEVICE_UNAVAILABLE;
  }

  if (connector_id) {
    drmtap_display displays[64] = {};
    const int count = lib->list_displays(ctx, displays, 64);
    for (int i = 0; i < count && i < 64; ++i) {
      if (displays[i].connector_id == connector_id && displays[i].active) {
        config.crtc_id = displays[i].crtc_id;
        break;
      }
    }
    if (!config.crtc_id)
      return KMS_NO_SCANOUT;
    crtc_id = config.crtc_id;
    lib->close(ctx);
    ctx = lib->open(&config);
    if (!ctx) {
      log_drmtap_error(lib, nullptr, "selected CRTC open failed");
      return KMS_DEVICE_UNAVAILABLE;
    }
  }

  // Establish the display configuration before publishing any captured frame.
  lib->displays_changed(ctx);

  int ret = lib->grab_desc(ctx, &first.desc, &first.info);
  if (ret < 0) {
    log_drmtap_error(lib, ctx, "drmtap grab failed");
    return ret == -ENODEV   ? KMS_NO_SCANOUT
           : ret == -EACCES ? KMS_HELPER_UNAVAILABLE
                            : KMS_PIPELINE_FAILED;
  }
  if (first.desc.width < MIN_SCANOUT_SIDE ||
      first.desc.height < MIN_SCANOUT_SIDE) {
    LOG_ERROR("the scanout is " + std::to_string(first.desc.width) + "x" +
              std::to_string(first.desc.height) + ", not a desktop yet");
    release(first);
    return KMS_NO_SCANOUT;
  }
  return KMS_OPENED;
}

void KmsCapture::close() {
  if (ctx)
    lib->close(ctx);
  ctx = nullptr;
}

bool KmsCapture::adopt(const drmtap_dmabuf_desc &desc) {
  width = int(desc.width);
  height = int(desc.height);
  fourcc = desc.format;
  modifier = desc.modifier;
  opened_modifier = desc.modifier;
  // A later grab is refused unless it has this size and format; one with
  // another modifier measures its buffer again (grab).
  off_t size = lseek(desc.dma_buf_fd, 0, SEEK_END);
  if (size < 0) {
    LOG_ERROR("dma-buf size query failed");
    return false;
  }
  buffer_size = size_t(size);
  return true;
}

void KmsCapture::start_pacing(int fps) {
  period = std::chrono::microseconds(1000000 / fps);
  next_frame = std::chrono::steady_clock::now();
}

void KmsCapture::set_pacing(int fps) {
  period = std::chrono::microseconds(1000000 / fps);
  next_frame =
      std::min(next_frame, std::chrono::steady_clock::now() + period);
}

void KmsCapture::wait_for_frame() {
  // A grab returns the current scanout at once, so this paces the capture at
  // the configured rate.
  std::this_thread::sleep_until(next_frame);
  auto now = std::chrono::steady_clock::now();
  next_frame += period;
  if (next_frame < now)
    next_frame = now;
}

int32_t KmsCapture::grab(KmsFrame &frame) {
  // A compositor can reassign a CRTC without changing the scanout size. The
  // caller rebuilds the capture, and the open resolves the connector's CRTC
  // again.
  if (lib->displays_changed(ctx) != 0)
    return KMS_PULL_SCANOUT_CHANGED;

  int ret = lib->grab_desc(ctx, &frame.desc, &frame.info);
  if (ret < 0) {
    log_drmtap_error(lib, ctx, "drmtap grab failed");
    return KMS_PULL_GRAB_FAILED;
  }
  // A scanout of another size or format is another display configuration;
  // the caller rebuilds the pipeline for it.
  if (int(frame.desc.width) != width || int(frame.desc.height) != height ||
      frame.desc.format != fourcc) {
    release(frame);
    return KMS_PULL_SCANOUT_CHANGED;
  }
  // An X server flips between its screen pixmap and a flip buffer of the
  // same size and format with another tiling modifier, for example when a
  // window goes full screen or a flip fails. Each frame carries its own
  // modifier into the encoder's import, so the stream goes on and only the
  // buffer's size is measured again. An import that fails while the
  // modifier is not the one the pipeline was built with rebuilds the
  // pipeline.
  if (frame.desc.modifier != modifier) {
    off_t size = lseek(frame.desc.dma_buf_fd, 0, SEEK_END);
    if (size < 0) {
      release(frame);
      LOG_ERROR("dma-buf size query failed");
      return KMS_PULL_SCANOUT_CHANGED;
    }
    modifier = frame.desc.modifier;
    buffer_size = size_t(size);
  }
  return KMS_PULL_ENCODED;
}

void KmsCapture::release(KmsFrame &frame) {
  lib->frame_release(ctx, &frame.info);
}

KmsCursor::~KmsCursor() {
  if (fd >= 0)
    ::close(fd);
}

void KmsCursor::open(const char *device, uint32_t crtc_id) {
  crtc = crtc_id;
  if (!device || !device[0])
    return;
  fd = ::open(device, O_RDWR | O_CLOEXEC);
  // Cursor planes are listed only to universal-plane clients, and their
  // CRTC_X/Y only to atomic ones. Neither takes DRM master.
  if (fd >= 0 && (drmSetClientCap(fd, DRM_CLIENT_CAP_UNIVERSAL_PLANES, 1) != 0 ||
                  drmSetClientCap(fd, DRM_CLIENT_CAP_ATOMIC, 1) != 0 ||
                  !load_planes())) {
    ::close(fd);
    fd = -1;
  }
  if (fd < 0)
    LOG_ERROR("cursor plane properties unreadable; the cursor comes through "
             "libdrmtap on every frame");
}

bool KmsCursor::bound(uint64_t plane_crtc) const {
  return crtc ? plane_crtc == crtc : plane_crtc != 0;
}

bool KmsCursor::load_planes() {
  drmModePlaneRes *resources = drmModeGetPlaneResources(fd);
  if (!resources)
    return false;
  for (uint32_t i = 0; i < resources->count_planes; ++i) {
    uint32_t id = resources->planes[i];
    drmModeObjectProperties *props =
        drmModeObjectGetProperties(fd, id, DRM_MODE_OBJECT_PLANE);
    if (!props)
      continue;
    bool cursor = false;
    for (uint32_t p = 0; p < props->count_props; ++p) {
      drmModePropertyRes *prop = drmModeGetProperty(fd, props->props[p]);
      if (!prop)
        continue;
      std::string name = prop->name;
      if (name == "type")
        cursor = props->prop_values[p] == DRM_PLANE_TYPE_CURSOR;
      else if (name == "FB_ID")
        prop_fb = prop->prop_id;
      else if (name == "CRTC_ID")
        prop_crtc = prop->prop_id;
      else if (name == "CRTC_X")
        prop_x = prop->prop_id;
      else if (name == "CRTC_Y")
        prop_y = prop->prop_id;
      drmModeFreeProperty(prop);
    }
    drmModeFreeObjectProperties(props);
    if (cursor)
      planes.push_back(id);
  }
  drmModeFreePlaneResources(resources);
  return !planes.empty() && prop_fb && prop_crtc && prop_x && prop_y;
}

bool KmsCursor::plane_state(uint32_t plane, uint64_t &fb, uint64_t &plane_crtc,
                            int32_t &x, int32_t &y) {
  drmModeObjectProperties *props =
      drmModeObjectGetProperties(fd, plane, DRM_MODE_OBJECT_PLANE);
  if (!props)
    return false;
  fb = plane_crtc = 0;
  x = y = 0;
  for (uint32_t p = 0; p < props->count_props; ++p) {
    const uint64_t value = props->prop_values[p];
    if (props->props[p] == prop_fb)
      fb = value;
    else if (props->props[p] == prop_crtc)
      plane_crtc = value;
    else if (props->props[p] == prop_x)
      x = int32_t(value);
    else if (props->props[p] == prop_y)
      y = int32_t(value);
  }
  drmModeFreeObjectProperties(props);
  return true;
}

int KmsCursor::fetch_image(const KmsCapture &capture) {
  drmtap_cursor_info fetched = {};
  int ret = capture.lib->get_cursor(capture.ctx, &fetched);
  if (ret >= 0) {
    image = fetched;
    image.pixels = nullptr;
    if (fetched.visible && fetched.pixels)
      pixels.assign(fetched.pixels,
                    fetched.pixels + size_t(fetched.width) * fetched.height);
    else
      image.visible = 0;
  }
  capture.lib->cursor_release(capture.ctx, &fetched);
  return ret;
}

int KmsCursor::read(const KmsCapture &capture, drmtap_cursor_info &out) {
  out = {};
  if (fd < 0) {
    int ret = fetch_image(capture);
    if (ret < 0 || !image.visible)
      return ret;
    out = image;
    out.pixels = pixels.data();
    return 0;
  }
  // The plane bound last is checked first; another plane is looked for only
  // when that one has left the CRTC, as when the cursor hides.
  uint64_t fb = 0, plane_crtc = 0;
  int32_t x = 0, y = 0;
  bool found = false;
  for (size_t i = 0; i < planes.size() && !found; ++i) {
    found = plane_state(planes[i], fb, plane_crtc, x, y) && fb && bound(plane_crtc);
    if (found && i > 0)
      std::swap(planes[0], planes[i]);
  }
  // A hidden cursor drops the kept image, so the next one shown is read
  // again even under the same framebuffer.
  if (!found) {
    image_fb = 0;
    return 0;
  }
  const auto now = std::chrono::steady_clock::now();
  if (fb != image_fb || now - image_at >= CURSOR_REFRESH) {
    image_fb = 0;
    int ret = fetch_image(capture);
    if (ret < 0)
      return ret;
    if (!image.visible)
      return 0;
    image_fb = fb;
    image_at = now;
  }
  out = image;
  out.visible = 1;
  out.x = x;
  out.y = y;
  out.pixels = pixels.data();
  return 0;
}

extern "C" int kms_list_displays(const char *device, drmtap_display *out,
                                 int count) {
  const Drmtap *lib = drmtap();
  if (!lib)
    return -1;
  drmtap_config config = {};
  config.device_path = device;
  drmtap_ctx *ctx = lib->open(&config);
  if (!ctx)
    return -1;
  const int result = lib->list_displays(ctx, out, count);
  lib->close(ctx);
  return result;
}

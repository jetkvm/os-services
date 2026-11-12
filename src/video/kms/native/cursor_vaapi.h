#pragma once

#include <algorithm>
#include <cstring>
#include <vector>
extern "C" {
#include <libavutil/hwcontext_vaapi.h>
#include <va/va_vpp.h>
}

#include "cursor_gl.h"

// Blend the hardware cursor before converting the desktop to encoder NV12.
// Desktop pixels stay on the GPU, including on drivers without VA blending.
class CursorVpp {
 public:
  AVBufferRef *frames = nullptr;

  ~CursorVpp() {
    if (context != VA_INVALID_ID)
      vaDestroyContext(display, context);
    if (config != VA_INVALID_ID)
      vaDestroyConfig(display, config);
    av_frame_free(&rgb);
    av_buffer_unref(&rgb_frames);
    av_buffer_unref(&frames);
  }

  int init(AVBufferRef *input, int width, int height, const char *render_node) {
    auto source = reinterpret_cast<AVHWFramesContext *>(input->data);
    auto va = static_cast<AVVAAPIDeviceContext *>(source->device_ctx->hwctx);
    display = va->display;
    frames = make_frames(source->device_ref, AV_PIX_FMT_NV12, width, height);
    if (!frames)
      return AVERROR(ENOMEM);
    if (!ok(vaCreateConfig(display, VAProfileNone, VAEntrypointVideoProc,
                           nullptr, 0, &config), "create cursor VPP config") ||
        !ok(vaCreateContext(display, config, width, height, VA_PROGRESSIVE,
                            nullptr, 0, &context), "create cursor VPP context"))
      return AVERROR(EIO);
    rgb_frames = make_frames(source->device_ref, AV_PIX_FMT_BGRA, width, height);
    rgb = av_frame_alloc();
    if (!rgb_frames || !rgb || !gl.init(render_node)) {
      LOG_ERROR("cursor EGL initialization failed");
      return AVERROR(EIO);
    }
    return 0;
  }

  int compose(AVFrame *input, AVFrame *prime, AVFrame *output,
              const drmtap_cursor_info &cursor) {
    av_frame_unref(output);
    int ret = av_hwframe_get_buffer(frames, output, 0);
    if (ret < 0)
      return ret;
    if ((ret = av_frame_copy_props(output, input)) < 0)
      return ret;

    VASurfaceID desktop_surface = surface(input);
    av_frame_unref(rgb);
    if (cursor.visible && cursor.pixels && cursor.width && cursor.height) {
      if (cursor.width > 1024 || cursor.height > 1024)
        return AVERROR(EINVAL);
      if ((ret = av_hwframe_get_buffer(rgb_frames, rgb, 0)) < 0)
        return ret;
      if (!gl.compose(display, reinterpret_cast<AVDRMFrameDescriptor *>(prime->data[0]), surface(rgb), input->width, input->height, cursor)) {
        LOG_ERROR("cursor EGL composition failed");
        return AVERROR(EIO);
      }
      desktop_surface = surface(rgb);
    }
    VARectangle desktop = {0, 0, uint16_t(input->width), uint16_t(input->height)};
    VAProcPipelineParameterBuffer params = {};
    params.surface = desktop_surface;
    params.surface_region = &desktop;
    params.output_region = &desktop;
    params.output_background_color = 0xff000000;
    params.surface_color_standard = VAProcColorStandardNone;
    params.output_color_standard = VAProcColorStandardBT601;
    params.filter_flags = VA_FRAME_PICTURE;
    VABufferID buffer = VA_INVALID_ID;
    if (!ok(vaCreateBuffer(display, context, VAProcPipelineParameterBufferType,
                           sizeof(params), 1, &params, &buffer),
            "create cursor VPP parameters"))
      return AVERROR(EIO);
    bool submitted = ok(vaBeginPicture(display, context, surface(output)),
                         "begin cursor VPP picture");
    if (submitted) {
      submitted = ok(vaRenderPicture(display, context, &buffer, 1),
                     "render cursor VPP picture");
      submitted = ok(vaEndPicture(display, context), "end cursor VPP picture") && submitted;
    }
    vaDestroyBuffer(display, buffer);
    return submitted ? 0 : AVERROR(EIO);
  }

 private:
  VADisplay display = nullptr;
  VAConfigID config = VA_INVALID_ID;
  VAContextID context = VA_INVALID_ID;
  AVBufferRef *rgb_frames = nullptr;
  AVFrame *rgb = nullptr;
  CursorGL gl;

  static VASurfaceID surface(const AVFrame *frame) {
    return VASurfaceID(uintptr_t(frame->data[3]));
  }

  static bool ok(VAStatus status, const char *operation) {
    if (status == VA_STATUS_SUCCESS)
      return true;
    LOG_ERROR(std::string(operation) + ": " + vaErrorStr(status));
    return false;
  }

  static AVBufferRef *make_frames(AVBufferRef *device, AVPixelFormat format,
                                   int width, int height) {
    AVBufferRef *ref = av_hwframe_ctx_alloc(device);
    if (!ref)
      return nullptr;
    auto ctx = reinterpret_cast<AVHWFramesContext *>(ref->data);
    ctx->format = AV_PIX_FMT_VAAPI;
    ctx->sw_format = format;
    ctx->width = width;
    ctx->height = height;
    if (av_hwframe_ctx_init(ref) < 0)
      av_buffer_unref(&ref);
    return ref;
  }
};

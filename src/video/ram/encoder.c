// The hardware encoders of Windows and macOS through FFmpeg, for encoder.rs:
// NV12 frames in, H.264 or H.265 packets out. Each vendor's encoder gets the
// options that keep it low-latency. The bitrate changes in place, and the
// next frame can be forced to an IDR. The encoders schedule no IDR.

#include <libavcodec/avcodec.h>
#include <libavutil/hwcontext.h>
#include <libavutil/opt.h>
#include <libavutil/time.h>
#include <limits.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

typedef void (*ipkvm_packet_fn)(void *context, const uint8_t *data, int size, int key);

typedef struct ipkvm_encoder {
  AVCodecContext *codec;
  // The caller's NV12 buffer, described by the layout that open returns.
  AVFrame *frame;
  // NVENC on Windows encodes from a D3D11 frame that frame is uploaded to.
  AVFrame *upload;
  AVPacket *packet;
  int qsv;
  // AMF: encode waits for the packet of each frame (see ipkvm_encoder_encode).
  int amf;
  int force_idr;
} ipkvm_encoder;

static int named(const char *name, const char *part) {
  return strstr(name, part) != NULL;
}

static int set(void *options, const char *key, const char *value) {
  int ret = av_opt_set(options, key, value, 0);
  if (ret < 0)
    fprintf(stderr, "encoder option %s=%s refused: %s\n", key, value, av_err2str(ret));
  return ret;
}

// The options of each vendor's encoder: rate control that follows the
// content, no frame held back for lookahead or pipelining, and an IDR for a
// frame of picture type I.
static int set_vendor_options(AVCodecContext *c, const char *name) {
  void *options = c->priv_data;
  if (named(name, "nvenc"))
    return set(options, "delay", "0") < 0 || set(options, "rc", "vbr") < 0 ||
                   set(options, "forced-idr", "1") < 0
               ? -1
               : 0;
  if (named(name, "amf"))
    return set(options, "usage", "ultralowlatency") < 0 || set(options, "rc", "vbr_latency") < 0 ||
                   set(options, "forced_idr", "1") < 0
               ? -1
               : 0;
  if (named(name, "qsv")) {
    c->strict_std_compliance = FF_COMPLIANCE_UNOFFICIAL;
    return set(options, "async_depth", "1") < 0 || set(options, "forced_idr", "1") < 0 ? -1 : 0;
  }
  if (named(name, "videotoolbox"))
    return set(options, "realtime", "1") < 0 || set(options, "prio_speed", "1") < 0 ||
                   set(options, "allow_sw", "0") < 0
               ? -1
               : 0;
  return 0;
}

// The encoders of FFmpeg's port in vendor/vcpkg apply a new bit_rate to the
// running session.
void ipkvm_encoder_set_bitrate(ipkvm_encoder *e, int kbps) {
  e->codec->bit_rate = (int64_t)kbps * 1000;
  // Quick Sync takes a maximum just above the target as variable rate.
  if (e->qsv) {
    e->codec->rc_max_rate = e->codec->bit_rate;
    e->codec->bit_rate--;
  }
}

#ifdef _WIN32
// NVENC on Windows encodes from D3D11 frames on the first NVIDIA adapter.
static int open_upload(ipkvm_encoder *e) {
  AVDictionary *options = NULL;
  av_dict_set(&options, "vendor_id", "0x10de", 0);
  AVBufferRef *hw_device = NULL;
  int ret = av_hwdevice_ctx_create(&hw_device, AV_HWDEVICE_TYPE_D3D11VA, NULL, options, 0);
  av_dict_free(&options);
  if (ret < 0)
    return ret;
  AVBufferRef *frames = av_hwframe_ctx_alloc(hw_device);
  av_buffer_unref(&hw_device);
  if (!frames)
    return AVERROR(ENOMEM);
  AVHWFramesContext *context = (AVHWFramesContext *)frames->data;
  context->format = AV_PIX_FMT_D3D11;
  context->sw_format = AV_PIX_FMT_NV12;
  context->width = e->codec->width;
  context->height = e->codec->height;
  context->initial_pool_size = 1;
  if ((ret = av_hwframe_ctx_init(frames)) < 0) {
    av_buffer_unref(&frames);
    return ret;
  }
  e->codec->hw_frames_ctx = frames;
  e->codec->pix_fmt = AV_PIX_FMT_D3D11;
  if (!(e->upload = av_frame_alloc()))
    return AVERROR(ENOMEM);
  return av_hwframe_get_buffer(frames, e->upload, 0);
}
#endif

void ipkvm_encoder_close(ipkvm_encoder *e) {
  if (!e)
    return;
  av_packet_free(&e->packet);
  av_frame_free(&e->frame);
  av_frame_free(&e->upload);
  avcodec_free_context(&e->codec);
  av_free(e);
}

// Opens the encoder `name` for width x height NV12 frames and writes the NV12
// layout that encode reads. Returns NULL when the encoder does not open.
ipkvm_encoder *ipkvm_encoder_open(const char *name, int width, int height, int fps, int kbps,
                                  int *y_stride, int *uv_offset, int *uv_stride) {
  av_log_set_level(AV_LOG_ERROR);
  const AVCodec *codec = avcodec_find_encoder_by_name(name);
  ipkvm_encoder *e = av_mallocz(sizeof *e);
  if (!codec || !e || width % 2 || height % 2)
    goto fail;
  if (!(e->codec = avcodec_alloc_context3(codec)))
    goto fail;
  AVCodecContext *c = e->codec;
  e->qsv = named(name, "qsv");
  e->amf = named(name, "amf");
  c->width = width;
  c->height = height;
  c->pix_fmt = AV_PIX_FMT_NV12;
  c->sw_pix_fmt = AV_PIX_FMT_NV12;
#ifdef _WIN32
  if (named(name, "nvenc") && open_upload(e) < 0)
    goto fail;
#endif
  c->max_b_frames = 0;
  // Quick Sync counts its IDR interval in 16 bits.
  c->gop_size = e->qsv ? UINT16_MAX : INT_MAX;
  c->keyint_min = INT_MAX;
  ipkvm_encoder_set_bitrate(e, kbps);
  c->time_base = av_make_q(1, 1000);
  c->framerate = av_make_q(fps, 1);
  c->flags |= AV_CODEC_FLAG_LOW_DELAY;
  c->slices = 1;
  c->thread_type = FF_THREAD_SLICE;
  c->thread_count = 1;
  // BT.601 limited range, as the captures convert.
  c->color_range = AVCOL_RANGE_MPEG;
  c->colorspace = AVCOL_SPC_SMPTE170M;
  c->color_primaries = AVCOL_PRI_SMPTE170M;
  c->color_trc = AVCOL_TRC_SMPTE170M;
  c->profile = named(name, "h264") ? AV_PROFILE_H264_HIGH : AV_PROFILE_HEVC_MAIN;
  if (set_vendor_options(c, name) < 0)
    goto fail;
  int ret = avcodec_open2(c, codec, NULL);
  if (ret < 0) {
    fprintf(stderr, "%s did not open: %s\n", name, av_err2str(ret));
    goto fail;
  }
  if (!(e->frame = av_frame_alloc()) || !(e->packet = av_packet_alloc()))
    goto fail;
  e->frame->format = AV_PIX_FMT_NV12;
  e->frame->width = width;
  e->frame->height = height;
  if (av_frame_get_buffer(e->frame, 0) < 0)
    goto fail;
  *y_stride = e->frame->linesize[0];
  *uv_stride = e->frame->linesize[1];
  *uv_offset = e->frame->linesize[0] * height;
  return e;
fail:
  ipkvm_encoder_close(e);
  return NULL;
}

// Encodes one NV12 frame and hands each packet the encoder has ready to
// `packet`; on AMF, up to and including the packet of this frame. Returns 0
// or a negative AVERROR.
int ipkvm_encoder_encode(ipkvm_encoder *e, const uint8_t *nv12, int length, int64_t pts,
                         ipkvm_packet_fn packet, void *context) {
  AVFrame *frame = e->frame;
  int ret;
  if (length < frame->linesize[0] * frame->height + frame->linesize[1] * frame->height / 2)
    return AVERROR(EINVAL);
  frame->data[0] = (uint8_t *)nv12;
  frame->data[1] = (uint8_t *)nv12 + frame->linesize[0] * frame->height;
  if (e->upload) {
    if ((ret = av_hwframe_transfer_data(e->upload, frame, 0)) < 0)
      return ret;
    frame = e->upload;
  }
  frame->pict_type = e->force_idr ? AV_PICTURE_TYPE_I : AV_PICTURE_TYPE_NONE;
  frame->pts = pts;
  e->force_idr = 0;
  if ((ret = avcodec_send_frame(e->codec, frame)) < 0)
    return ret;
  // On AMF, the frame's packet is waited for, for up to a second. AMF has
  // the packet only some time after the frame, and FFmpeg asks it again only
  // when called again; a frame that left with a later one would add a frame
  // of latency for as long as the encoder runs. FFmpeg's query waits up to
  // 1 ms in a driver that supports AMF_VIDEO_ENCODER_QUERY_TIMEOUT; after a
  // query that returned at once, the loop sleeps 1 ms instead. The other
  // encoders hand out each packet at once, and VideoToolbox drops frames
  // under load, which a wait would stall on.
  int64_t deadline = av_gettime_relative() + 1000000;
  int done = !e->amf;
  for (;;) {
    int64_t asked = av_gettime_relative();
    ret = avcodec_receive_packet(e->codec, e->packet);
    if (ret == AVERROR(EAGAIN)) {
      int64_t now = av_gettime_relative();
      if (done || now >= deadline)
        return 0;
      if (now - asked < 500)
        av_usleep(1000);
      continue;
    }
    if (ret < 0)
      return ret;
    done |= e->packet->pts == pts;
    packet(context, e->packet->data, e->packet->size, e->packet->flags & AV_PKT_FLAG_KEY);
    av_packet_unref(e->packet);
  }
}

void ipkvm_encoder_request_keyframe(ipkvm_encoder *e) { e->force_idr = 1; }

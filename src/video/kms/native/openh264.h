// H.264 in software with Cisco's OpenH264, for the VAAPI pipeline when the
// driver has no H.264 encoder, as Fedora's Mesa has none. The library is the
// one the computer has installed, for example Fedora's openh264 package from
// Cisco's repository: the service loads it at run time and never ships it.
//
// The headers in openh264/ are those of release 2.5.1, the ABI of soname 7.
// Soname 8 (release 2.6 and later) appends fields to the encoder
// parameters, the source picture and the per-layer output. The parameters
// and the picture get zeroed room for those fields, and the output is read
// with the layout of the library that loaded.

#ifndef KMS_OPENH264_H
#define KMS_OPENH264_H

#include <dlfcn.h>

#include <algorithm>
#include <cstdint>
#include <cstring>
#include <functional>
#include <string>
#include <thread>
#include <vector>

extern "C" {
#include <libavutil/frame.h>
}

#include "openh264/codec_api.h"

class Openh264 {
public:
  using Deliver = std::function<void(const uint8_t *data, int size, bool key)>;

  // OpenH264 clamps its rate budget to 60 frames a second, so frames that
  // come faster overshoot the bitrate; the capture paces at most this.
  static constexpr int kMaxFps = 60;

  Openh264() = default;
  Openh264(const Openh264 &) = delete;
  Openh264 &operator=(const Openh264 &) = delete;

  ~Openh264() {
    if (encoder_) {
      encoder_->Uninitialize();
      destroy_(encoder_);
    }
    if (library_)
      dlclose(library_);
  }

  // Loads the computer's OpenH264 and opens a width x height encoder at
  // `kbps`. False, with the reason in the log, when there is no usable
  // library or it refuses the size.
  bool open(int width, int height, int fps, int kbps) {
    if (!load())
      return false;
    if (create_(&encoder_) != 0 || !encoder_) {
      LOG_ERROR("OpenH264 created no encoder");
      return false;
    }
    // OpenH264 writes its notes on the parameters to stderr; only its
    // errors belong in the service's log.
    int trace = WELS_LOG_ERROR;
    encoder_->SetOption(ENCODER_OPTION_TRACE_LEVEL, &trace);
    Room<SEncParamExt> params;
    encoder_->GetDefaultParams(&params.value);
    SEncParamExt &p = params.value;
    // The camera mode encodes a full screen of changing text at 1920x1080 in
    // 7.6 ms, the screen-content mode in 26 ms at a third of the bitrate;
    // the KVM adapts the bitrate, and latency counts more.
    p.iUsageType = CAMERA_VIDEO_REAL_TIME;
    p.iPicWidth = width;
    p.iPicHeight = height;
    p.iRCMode = RC_BITRATE_MODE;
    p.iTargetBitrate = kbps * 1000;
    p.iMaxBitrate = UNSPECIFIED_BIT_RATE;
    p.fMaxFrameRate = frame_rate(fps);
    p.iTemporalLayerNum = 1;
    p.iSpatialLayerNum = 1;
    // The KVM asks for an IDR when a viewer joins or loses frames; the
    // encoder adds none of its own, and keeps every frame.
    p.uiIntraPeriod = 0;
    p.bEnableSceneChangeDetect = false;
    p.bEnableFrameSkip = false;
    p.eSpsPpsIdStrategy = CONSTANT_ID;
    p.iComplexityMode = LOW_COMPLEXITY;
    int threads = std::clamp(int(std::thread::hardware_concurrency()) / 2, 1, 4);
    p.iMultipleThreadIdc = static_cast<unsigned short>(threads);
    SSpatialLayerConfig &layer = p.sSpatialLayers[0];
    layer.iVideoWidth = width;
    layer.iVideoHeight = height;
    layer.fFrameRate = p.fMaxFrameRate;
    layer.iSpatialBitrate = p.iTargetBitrate;
    layer.iMaxSpatialBitrate = UNSPECIFIED_BIT_RATE;
    // One slice per thread, so that the threads share each frame.
    layer.sSliceArgument.uiSliceMode = threads > 1 ? SM_FIXEDSLCNUM_SLICE : SM_SINGLE_SLICE;
    layer.sSliceArgument.uiSliceNum = unsigned(threads);
    int ret = encoder_->InitializeExt(&p);
    if (ret != 0) {
      LOG_ERROR("OpenH264 refused " + std::to_string(width) + "x" + std::to_string(height) +
                ": " + std::to_string(ret));
      return false;
    }
    int format = videoFormatI420;
    encoder_->SetOption(ENCODER_OPTION_DATAFORMAT, &format);
    width_ = width;
    height_ = height;
    u_.resize(size_t(width / 2) * size_t(height / 2));
    v_.resize(u_.size());
    return true;
  }

  // The encoder's bitrate and frame rate change in place.
  void set_rate(int fps, int kbps) {
    SBitrateInfo bitrate = {SPATIAL_LAYER_ALL, kbps * 1000};
    encoder_->SetOption(ENCODER_OPTION_BITRATE, &bitrate);
    float rate = frame_rate(fps);
    encoder_->SetOption(ENCODER_OPTION_FRAME_RATE, &rate);
  }

  // Encodes one NV12 frame in memory and hands its access unit to `deliver`.
  // Returns 0, or the encoder's error.
  int encode(const AVFrame *nv12, bool force_idr, uint64_t pts_us, const Deliver &deliver) {
    split_chroma(nv12);
    Room<SSourcePicture> picture;
    SSourcePicture &pic = picture.value;
    pic.iColorFormat = videoFormatI420;
    pic.iPicWidth = width_;
    pic.iPicHeight = height_;
    pic.iStride[0] = nv12->linesize[0];
    pic.iStride[1] = pic.iStride[2] = width_ / 2;
    pic.pData[0] = nv12->data[0];
    pic.pData[1] = u_.data();
    pic.pData[2] = v_.data();
    pic.uiTimeStamp = (long long)(pts_us / 1000);
    if (force_idr)
      encoder_->ForceIntraFrame(true);
    return wide_layers_ ? encode_with<LayerWithPsnr>(pic, deliver)
                        : encode_with<SLayerBSInfo>(pic, deliver);
  }

private:
  // A struct with zeroed room behind it for the fields a newer library
  // appends.
  template <typename T> struct Room {
    T value;
    unsigned char room[64];
    Room() { std::memset(this, 0, sizeof(*this)); }
  };

  // The per-layer output of soname 8: soname 7's, then the Y, U and V PSNR.
  struct LayerWithPsnr {
    SLayerBSInfo layer;
    float psnr[3];
  };
  template <typename Layer> struct FrameInfo {
    int iLayerNum;
    Layer sLayerInfo[MAX_LAYER_NUM_OF_FRAME];
    EVideoFrameType eFrameType;
    int iFrameSizeInBytes;
    long long uiTimeStamp;
  };
  static const SLayerBSInfo &base(const SLayerBSInfo &layer) { return layer; }
  static const SLayerBSInfo &base(const LayerWithPsnr &layer) { return layer.layer; }

  template <typename Layer> int encode_with(SSourcePicture &pic, const Deliver &deliver) {
    std::vector<unsigned char> &out = frame_info_;
    out.assign(sizeof(FrameInfo<Layer>), 0);
    auto *info = reinterpret_cast<FrameInfo<Layer> *>(out.data());
    int ret = encoder_->EncodeFrame(&pic, reinterpret_cast<SFrameBSInfo *>(info));
    if (ret != 0)
      return ret;
    if (info->eFrameType == videoFrameTypeSkip || info->iLayerNum <= 0)
      return 0;
    unit_.clear();
    for (int i = 0; i < info->iLayerNum && i < MAX_LAYER_NUM_OF_FRAME; i++) {
      const SLayerBSInfo &layer = base(info->sLayerInfo[i]);
      size_t size = 0;
      for (int n = 0; n < layer.iNalCount; n++)
        size += size_t(layer.pNalLengthInByte[n]);
      unit_.insert(unit_.end(), layer.pBsBuf, layer.pBsBuf + size);
    }
    if (!unit_.empty())
      deliver(unit_.data(), int(unit_.size()), info->eFrameType == videoFrameTypeIDR);
    return 0;
  }

  // OpenH264 takes I420: the U and V samples of NV12's interleaved plane
  // go to planes of their own.
  void split_chroma(const AVFrame *nv12) {
    const int cw = width_ / 2, ch = height_ / 2;
    for (int y = 0; y < ch; y++) {
      const uint8_t *row = nv12->data[1] + size_t(y) * size_t(nv12->linesize[1]);
      uint8_t *u = u_.data() + size_t(y) * size_t(cw);
      uint8_t *v = v_.data() + size_t(y) * size_t(cw);
      for (int x = 0; x < cw; x++) {
        u[x] = row[2 * x];
        v[x] = row[2 * x + 1];
      }
    }
  }

  static float frame_rate(int fps) { return float(std::clamp(fps, 1, kMaxFps)); }

  // Opens soname 8 or 7, the ABIs this file reads, and checks the version.
  bool load() {
    for (const char *name : {"libopenh264.so.8", "libopenh264.so.7"}) {
      library_ = dlopen(name, RTLD_NOW | RTLD_LOCAL);
      if (library_)
        break;
    }
    if (!library_) {
      LOG_ERROR("no OpenH264 library (libopenh264.so.8 or .7)");
      return false;
    }
    auto version = reinterpret_cast<void (*)(OpenH264Version *)>(
        dlsym(library_, "WelsGetCodecVersionEx"));
    create_ = reinterpret_cast<int (*)(ISVCEncoder **)>(dlsym(library_, "WelsCreateSVCEncoder"));
    destroy_ = reinterpret_cast<void (*)(ISVCEncoder *)>(dlsym(library_, "WelsDestroySVCEncoder"));
    if (!version || !create_ || !destroy_) {
      LOG_ERROR("the OpenH264 library lacks its encoder functions");
      return false;
    }
    OpenH264Version v = {};
    version(&v);
    if (v.uMajor != 2 || v.uMinor < 4) {
      LOG_ERROR("OpenH264 " + std::to_string(v.uMajor) + "." + std::to_string(v.uMinor) +
                " is not 2.4 or later 2.x");
      return false;
    }
    wide_layers_ = v.uMinor >= 6;
    return true;
  }

  void *library_ = nullptr;
  int (*create_)(ISVCEncoder **) = nullptr;
  void (*destroy_)(ISVCEncoder *) = nullptr;
  ISVCEncoder *encoder_ = nullptr;
  bool wide_layers_ = false;
  int width_ = 0;
  int height_ = 0;
  std::vector<uint8_t> u_, v_;
  std::vector<unsigned char> frame_info_;
  std::vector<uint8_t> unit_;
};

#endif

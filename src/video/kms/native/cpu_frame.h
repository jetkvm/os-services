// The picture of the CPU path (kms_software.cpp) as OpenH264 reads it: the
// grabbed scanout converted to I420 on the CPU, with the hardware cursor
// blended in. Only the rows that changed since the last grab, and the
// cursor's rectangle, are converted again.
//
// The conversion is BT.601 at limited range, as the VAAPI conversion before
// the other software encode (CursorVpp) produces it, so the colors do not
// shift between the two paths.

#pragma once

#include <algorithm>
#include <cstddef>
#include <cstdint>
#include <cstring>
#include <vector>

// The cursor as the CPU path draws it: premultiplied ARGB8888 pixels with
// their top left corner at (x, y). As in cursor_gl.h, the position already
// includes the hotspot.
struct CpuPointer {
  bool visible = false;
  int x = 0;
  int y = 0;
  int width = 0;
  int height = 0;
  const uint32_t *pixels = nullptr;
};

class CpuFrame {
 public:
  // What an update changed.
  enum Change { UNCHANGED = 0, PICTURE = 1, POINTER = 2 };

  int width() const { return width_; }
  int height() const { return height_; }
  int chroma_width() const { return (width_ + 1) / 2; }
  const uint8_t *y() const { return y_.data(); }
  const uint8_t *u() const { return u_.data(); }
  const uint8_t *v() const { return v_.data(); }

  // Sizes the frame for a width x height scanout whose pixels are bytes
  // B, G, R, X (XRGB8888) or, with `red_first`, R, G, B, X (XBGR8888). The
  // next update converts the whole picture.
  void reset(int width, int height, bool red_first) {
    width_ = width;
    height_ = height;
    red_first_ = red_first;
    screen_.assign(size_t(width) * height * 4, 0);
    y_.assign(size_t(width) * height, 0);
    const size_t chroma = size_t(chroma_width()) * ((height + 1) / 2);
    u_.assign(chroma, 128);
    v_.assign(chroma, 128);
    drawn_ = {};
    shown_ = {};
    shown_visible_ = false;
    image_.clear();
    whole_ = true;
  }

  // Takes a grab of the scanout and the cursor over it. `pixels` is only
  // read: on a linear scanout it can be a mapping of the screen itself.
  Change update(const uint8_t *pixels, size_t stride, const CpuPointer &pointer) {
    // A compositor can draw into the framebuffer it scans out without a page
    // flip, so the framebuffer ID proves nothing: the rows themselves are
    // compared with the last grab, which also keeps a copy of them.
    const size_t row_bytes = size_t(width_) * 4;
    int first = height_, last = -1;
    for (int row = 0; row < height_; ++row) {
      const uint8_t *source = pixels + size_t(row) * stride;
      uint8_t *kept = screen_.data() + size_t(row) * row_bytes;
      if (whole_ || std::memcmp(source, kept, row_bytes) != 0) {
        std::memcpy(kept, source, row_bytes);
        first = std::min(first, row);
        last = row;
      }
    }
    whole_ = false;
    const bool picture = last >= 0;
    if (!picture && !pointer_changed(pointer))
      return UNCHANGED;
    if (picture) {
      // Whole row pairs, as each chroma row covers two.
      const int top = first & ~1;
      const int bottom = std::min((last + 2) & ~1, height_);
      convert(top, bottom - top, screen_.data() + size_t(top) * row_bytes, row_bytes, 0,
              width_);
    }
    // The cursor's old rectangle gets the screen back, and the cursor is
    // drawn again: a picture change can have converted rows under it.
    if (drawn_.width > 0)
      convert(drawn_.y, drawn_.height,
              screen_.data() + size_t(drawn_.y) * row_bytes + size_t(drawn_.x) * 4, row_bytes,
              drawn_.x, drawn_.width);
    draw(pointer);
    remember(pointer);
    return picture ? PICTURE : POINTER;
  }

 private:
  struct Rect {
    int x = 0, y = 0, width = 0, height = 0;
  };

  int width_ = 0;
  int height_ = 0;
  bool red_first_ = false;
  // The next update converts every row.
  bool whole_ = true;
  // The last grab, without the cursor, in the scanout's byte order.
  std::vector<uint8_t> screen_;
  std::vector<uint8_t> y_, u_, v_;
  // The rectangle the cursor was drawn into, on even coordinates.
  Rect drawn_;
  // The cursor last drawn, and a copy of its image.
  Rect shown_;
  bool shown_visible_ = false;
  std::vector<uint32_t> image_;
  // The cursor's rectangle with the screen under it, then the cursor on top.
  std::vector<uint8_t> patch_;

  static bool visible(const CpuPointer &pointer) {
    return pointer.visible && pointer.pixels && pointer.width > 0 && pointer.height > 0;
  }

  bool pointer_changed(const CpuPointer &pointer) const {
    if (!visible(pointer))
      return shown_visible_;
    return !shown_visible_ || pointer.x != shown_.x || pointer.y != shown_.y ||
           pointer.width != shown_.width || pointer.height != shown_.height ||
           std::memcmp(pointer.pixels, image_.data(), image_.size() * 4) != 0;
  }

  void remember(const CpuPointer &pointer) {
    shown_visible_ = visible(pointer);
    if (!shown_visible_)
      return;
    shown_ = {pointer.x, pointer.y, pointer.width, pointer.height};
    image_.assign(pointer.pixels, pointer.pixels + size_t(pointer.width) * pointer.height);
  }

  // Blends the cursor over a copy of the screen under it and converts that
  // copy. The rectangle grows to even coordinates, so that it covers whole
  // chroma samples, and is clipped at the screen's edges.
  void draw(const CpuPointer &pointer) {
    drawn_ = {};
    if (!visible(pointer))
      return;
    const int left = std::max(pointer.x, 0) & ~1;
    const int top = std::max(pointer.y, 0) & ~1;
    const int right = std::min((pointer.x + pointer.width + 1) & ~1, width_);
    const int bottom = std::min((pointer.y + pointer.height + 1) & ~1, height_);
    if (right <= left || bottom <= top)
      return;
    const Rect rect = {left, top, right - left, bottom - top};
    const size_t patch_stride = size_t(rect.width) * 4;
    patch_.resize(patch_stride * rect.height);
    const size_t screen_stride = size_t(width_) * 4;
    for (int row = 0; row < rect.height; ++row)
      std::memcpy(patch_.data() + row * patch_stride,
                  screen_.data() + size_t(rect.y + row) * screen_stride + size_t(rect.x) * 4,
                  patch_stride);
    const int red = red_first_ ? 0 : 2, blue = red_first_ ? 2 : 0;
    for (int row = std::max(0, rect.y - pointer.y);
         row < pointer.height && pointer.y + row < bottom; ++row) {
      const uint32_t *image = pointer.pixels + size_t(row) * pointer.width;
      uint8_t *line = patch_.data() + size_t(pointer.y + row - rect.y) * patch_stride;
      for (int column = std::max(0, rect.x - pointer.x);
           column < pointer.width && pointer.x + column < right; ++column) {
        const uint32_t argb = image[column];
        const uint32_t keep = 255 - (argb >> 24);
        uint8_t *pixel = line + size_t(pointer.x + column - rect.x) * 4;
        pixel[red] = blend(pixel[red], (argb >> 16) & 0xFF, keep);
        pixel[1] = blend(pixel[1], (argb >> 8) & 0xFF, keep);
        pixel[blue] = blend(pixel[blue], argb & 0xFF, keep);
      }
    }
    convert(rect.y, rect.height, patch_.data(), patch_stride, rect.x, rect.width);
    drawn_ = rect;
  }

  // Premultiplied "over": the cursor's channel plus the share of the screen
  // its alpha leaves, rounded.
  static uint8_t blend(uint32_t screen, uint32_t cursor, uint32_t keep) {
    const uint32_t under = (screen * keep + 127) / 255;
    return uint8_t(std::min<uint32_t>(cursor + under, 255));
  }

  void convert(int top, int rows, const uint8_t *source, size_t stride, int left, int columns) {
    if (red_first_)
      convert_rows<0, 2>(top, rows, source, stride, left, columns);
    else
      convert_rows<2, 0>(top, rows, source, stride, left, columns);
  }

  // BT.601 at limited range, in 16-bit fixed point: Y from 16 to 235, U and
  // V from 16 to 240. Each chroma sample is the mean of a 2x2 block; an odd
  // last row or column counts twice. `top` and `left` are even. Plain loops
  // over plain arrays, so that the compiler vectorizes them.
  template <int R, int B>
  void convert_rows(int top, int rows, const uint8_t *source, size_t stride, int left,
                    int columns) {
    const size_t chroma_stride = size_t(chroma_width());
    for (int row = 0; row < rows; row += 2) {
      const uint8_t *upper = source + size_t(row) * stride;
      const uint8_t *lower = row + 1 < rows ? upper + stride : upper;
      luma_row<R, B>(upper, y_.data() + size_t(top + row) * width_ + left, columns);
      if (lower != upper)
        luma_row<R, B>(lower, y_.data() + size_t(top + row + 1) * width_ + left, columns);
      const size_t chroma_row = size_t((top + row) / 2) * chroma_stride + size_t(left / 2);
      uint8_t *cb = u_.data() + chroma_row;
      uint8_t *cr = v_.data() + chroma_row;
      const int pairs = columns / 2;
      for (int x = 0; x < pairs; ++x)
        chroma<R, B>(upper + x * 8, upper + x * 8 + 4, lower + x * 8, lower + x * 8 + 4,
                     cb[x], cr[x]);
      if (columns & 1) {
        const int last = (columns - 1) * 4;
        chroma<R, B>(upper + last, upper + last, lower + last, lower + last, cb[pairs],
                     cr[pairs]);
      }
    }
  }

  template <int R, int B>
  static void luma_row(const uint8_t *line, uint8_t *luma, int columns) {
    for (int x = 0; x < columns; ++x) {
      const int r = line[x * 4 + R], g = line[x * 4 + 1], b = line[x * 4 + B];
      luma[x] = uint8_t(((16829 * r + 33039 * g + 6416 * b + 32768) >> 16) + 16);
    }
  }

  template <int R, int B>
  static void chroma(const uint8_t *a, const uint8_t *b, const uint8_t *c, const uint8_t *d,
                     uint8_t &cb, uint8_t &cr) {
    const int r = a[R] + b[R] + c[R] + d[R];
    const int g = a[1] + b[1] + c[1] + d[1];
    const int bl = a[B] + b[B] + c[B] + d[B];
    cb = uint8_t(((-9714 * r - 19070 * g + 28784 * bl + (1 << 17)) >> 18) + 128);
    cr = uint8_t(((28784 * r - 24103 * g - 4681 * bl + (1 << 17)) >> 18) + 128);
  }
};

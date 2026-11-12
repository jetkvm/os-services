#pragma once

#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <GLES2/gl2ext.h>
#include <sys/stat.h>
#include <unistd.h>

#include <cstring>
#include <string>
#include <vector>

extern "C" {
#include <drm_fourcc.h>
#include <libavutil/hwcontext_drm.h>
#include <va/va.h>
#include <va/va_drmcommon.h>
}

#include "drmtap.h"

// Render into an owned surface, never the host's scanout. EGL imports both
// surfaces on the capture GPU; the only CPU upload is the cursor image.
class CursorGL {
 public:
  // What drawing a scanout came to. An import EGL refuses means the scanout's
  // format or modifier is not one this GPU samples.
  enum class Drawn { Ok, ImportRefused, Failed };

  ~CursorGL() {
    if (context != EGL_NO_CONTEXT) {
      eglMakeCurrent(display, EGL_NO_SURFACE, EGL_NO_SURFACE, context);
      glDeleteTextures(1, &cursor_texture);
      glDeleteFramebuffers(1, &fbo);
      glDeleteProgram(program);
      eglMakeCurrent(display, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
      eglDestroyContext(display, context);
    }
    // EGLDisplays are shared with other contexts in the process.
  }

  // Creates a context on the EGL device behind `render_node` and makes it
  // current. Textures that CUDA registers need a sized internal format,
  // which needs `gles_version` 3.
  bool init(const char *render_node, int gles_version = 2) {
    auto devices = reinterpret_cast<PFNEGLQUERYDEVICESEXTPROC>(eglGetProcAddress("eglQueryDevicesEXT"));
    auto query = reinterpret_cast<PFNEGLQUERYDEVICESTRINGEXTPROC>(eglGetProcAddress("eglQueryDeviceStringEXT"));
    auto platform = reinterpret_cast<PFNEGLGETPLATFORMDISPLAYEXTPROC>(eglGetProcAddress("eglGetPlatformDisplayEXT"));
    create_image = reinterpret_cast<PFNEGLCREATEIMAGEKHRPROC>(eglGetProcAddress("eglCreateImageKHR"));
    destroy_image = reinterpret_cast<PFNEGLDESTROYIMAGEKHRPROC>(eglGetProcAddress("eglDestroyImageKHR"));
    bind_image = reinterpret_cast<PFNGLEGLIMAGETARGETTEXTURE2DOESPROC>(eglGetProcAddress("glEGLImageTargetTexture2DOES"));
    if (!devices || !query || !platform || !create_image || !destroy_image || !bind_image)
      return fail("EGL entry points");
    EGLint count = 0;
    if (!devices(0, nullptr, &count) || count <= 0)
      return fail("eglQueryDevicesEXT");
    std::vector<EGLDeviceEXT> found(count);
    if (!devices(count, found.data(), &count))
      return fail("eglQueryDevicesEXT");
    struct stat wanted = {};
    if (!render_node || stat(render_node, &wanted) != 0)
      return fail("render node");
    for (int i = 0; i < count; ++i) {
      auto path = query(found[i], EGL_DRM_RENDER_NODE_FILE_EXT);
      struct stat device = {};
      if (path && stat(path, &device) == 0 && wanted.st_rdev == device.st_rdev) {
        display = platform(EGL_PLATFORM_DEVICE_EXT, found[i], nullptr);
        break;
      }
    }
    if (display == EGL_NO_DISPLAY)
      return fail("EGL device for the render node");
    if (!eglInitialize(display, nullptr, nullptr) || !eglBindAPI(EGL_OPENGL_ES_API))
      return fail("eglInitialize");
    // The context draws only into textures. A GLES 3 config asks for no
    // surface type: the EGL device platform has no window surfaces.
    const EGLint es2_attrs[] = {EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
      EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT, EGL_NONE};
    const EGLint es3_attrs[] = {EGL_SURFACE_TYPE, 0,
      EGL_RENDERABLE_TYPE, EGL_OPENGL_ES3_BIT, EGL_NONE};
    EGLConfig config;
    if (!eglChooseConfig(display, gles_version >= 3 ? es3_attrs : es2_attrs, &config, 1,
                         &count) || count != 1)
      return fail("eglChooseConfig");
    const EGLint context_attrs[] = {EGL_CONTEXT_CLIENT_VERSION, gles_version >= 3 ? 3 : 2,
                                    EGL_NONE};
    context = eglCreateContext(display, config, EGL_NO_CONTEXT, context_attrs);
    if (context == EGL_NO_CONTEXT)
      return fail("eglCreateContext");
    if (!make_current())
      return fail("eglMakeCurrent");
    const char *vertex = "attribute vec2 position; attribute vec2 uv; varying vec2 tex;"
                         "void main(){gl_Position=vec4(position,0.,1.);tex=uv;}";
    const char *fragment = "precision mediump float; varying vec2 tex; uniform sampler2D image;"
                           "uniform bool cursor; void main(){vec4 c=texture2D(image,tex);"
                           "gl_FragColor=cursor?c.bgra:vec4(c.rgb,1.);}";
    GLuint vs = shader(GL_VERTEX_SHADER, vertex), fs = shader(GL_FRAGMENT_SHADER, fragment);
    if (!vs || !fs) {
      glDeleteShader(vs); glDeleteShader(fs);
      return false;
    }
    program = glCreateProgram();
    glAttachShader(program, vs); glAttachShader(program, fs);
    glBindAttribLocation(program, 0, "position"); glBindAttribLocation(program, 1, "uv");
    glLinkProgram(program);
    glDeleteShader(vs); glDeleteShader(fs);
    GLint linked = 0;
    glGetProgramiv(program, GL_LINK_STATUS, &linked);
    if (!linked)
      return false;
    glGenTextures(1, &cursor_texture); glGenFramebuffers(1, &fbo);
    glUseProgram(program);
    glUniform1i(glGetUniformLocation(program, "image"), 0);
    return glGetError() == GL_NO_ERROR;
  }

  bool compose(VADisplay va, const AVDRMFrameDescriptor *input, VASurfaceID output,
               int width, int height, const drmtap_cursor_info &cursor) {
    if (!eglMakeCurrent(display, EGL_NO_SURFACE, EGL_NO_SURFACE, context))
      return false;
    Imported source(this), target(this);
    // Complete any previous VA use before GL accesses these surfaces.
    if (vaSyncSurface(va, output) != VA_STATUS_SUCCESS ||
        !source.load_prime(input, width, height) ||
        !target.load(va, output, VA_EXPORT_SURFACE_WRITE_ONLY))
      return false;
    return draw(source.texture, target.texture, width, height, cursor);
  }

  bool make_current() {
    return eglMakeCurrent(display, EGL_NO_SURFACE, EGL_NO_SURFACE, context);
  }

  // Draws the scanout `desc` describes into `target`, a texture of this
  // context, with the cursor on top when it is visible.
  Drawn draw_scanout(const drmtap_dmabuf_desc &desc, GLuint target, int width, int height,
                     const drmtap_cursor_info &cursor) {
    if (!make_current())
      return Drawn::Failed;
    Imported source(this);
    if (!source.load_dmabuf(desc))
      return Drawn::ImportRefused;
    return draw(source.texture, target, width, height, cursor) ? Drawn::Ok : Drawn::Failed;
  }

 private:
  EGLDisplay display = EGL_NO_DISPLAY;
  EGLContext context = EGL_NO_CONTEXT;
  PFNEGLCREATEIMAGEKHRPROC create_image = nullptr;
  PFNEGLDESTROYIMAGEKHRPROC destroy_image = nullptr;
  PFNGLEGLIMAGETARGETTEXTURE2DOESPROC bind_image = nullptr;
  GLuint program = 0, fbo = 0, cursor_texture = 0;
  uint32_t cursor_width = 0, cursor_height = 0;
  std::vector<uint32_t> cached;

  static bool fail(const char *step) {
    LOG_ERROR(std::string("cursor EGL ") + step + " failed: EGL error " +
              std::to_string(eglGetError()));
    return false;
  }

  // Draws `source` over the whole of `target` and the cursor on top, and
  // waits until the GPU has finished.
  bool draw(GLuint source, GLuint target, int width, int height,
            const drmtap_cursor_info &cursor) {
    glBindFramebuffer(GL_FRAMEBUFFER, fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, target, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE)
      return false;
    glViewport(0, 0, width, height);
    glDisable(GL_BLEND);
    glUseProgram(program);
    glUniform1i(glGetUniformLocation(program, "cursor"), 0);
    glBindTexture(GL_TEXTURE_2D, source);
    quad(-1, -1, 1, 1);
    if (cursor.visible && cursor.pixels && cursor.width && cursor.height)
      blend_cursor(width, height, cursor);
    glFinish(); // VA and CUDA consume the completed result on another API/context.
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, 0, 0);
    return glGetError() == GL_NO_ERROR;
  }

  void blend_cursor(int width, int height, const drmtap_cursor_info &cursor) {
    glBindTexture(GL_TEXTURE_2D, cursor_texture);
    size_t pixels = size_t(cursor.width) * cursor.height;
    if (cursor_width != cursor.width || cursor_height != cursor.height ||
        cached.size() != pixels || std::memcmp(cached.data(), cursor.pixels, pixels * 4)) {
      texture_params();
      glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA, cursor.width, cursor.height, 0,
                   GL_RGBA, GL_UNSIGNED_BYTE, cursor.pixels);
      cached.assign(cursor.pixels, cursor.pixels + pixels);
      cursor_width = cursor.width; cursor_height = cursor.height;
    }
    glUniform1i(glGetUniformLocation(program, "cursor"), 1);
    glEnable(GL_BLEND);
    glBlendFunc(GL_ONE, GL_ONE_MINUS_SRC_ALPHA);
    // CRTC_X/Y already describe the image origin, including the hotspot.
    // DMA-BUF row zero and texture row zero share the same origin; matching
    // both quads preserves orientation and lets the viewport clip edges.
    quad(2.f * cursor.x / width - 1.f, 2.f * cursor.y / height - 1.f,
         2.f * (cursor.x + int(cursor.width)) / width - 1.f,
         2.f * (cursor.y + int(cursor.height)) / height - 1.f);
    glDisable(GL_BLEND);
  }

  static void texture_params() {
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
  }
  static GLuint shader(GLenum type, const char *source) {
    GLuint id = glCreateShader(type);
    glShaderSource(id, 1, &source, nullptr); glCompileShader(id);
    GLint compiled = 0; glGetShaderiv(id, GL_COMPILE_STATUS, &compiled);
    if (!compiled) { glDeleteShader(id); return 0; }
    return id;
  }
  static void quad(float left, float top, float right, float bottom) {
    GLfloat vertices[] = {left,top,0,0, right,top,1,0, left,bottom,0,1, right,bottom,1,1};
    glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 4*sizeof(GLfloat), vertices);
    glVertexAttribPointer(1, 2, GL_FLOAT, GL_FALSE, 4*sizeof(GLfloat), vertices+2);
    glEnableVertexAttribArray(0); glEnableVertexAttribArray(1);
    glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
    glDisableVertexAttribArray(0); glDisableVertexAttribArray(1);
  }
  struct Imported {
    CursorGL *owner;
    GLuint texture = 0;
    EGLImageKHR image = EGL_NO_IMAGE_KHR;
    explicit Imported(CursorGL *owner) : owner(owner) {}
    ~Imported() {
      glDeleteTextures(1, &texture);
      if (image != EGL_NO_IMAGE_KHR) owner->destroy_image(owner->display, image);
    }
    bool load(VADisplay va, VASurfaceID surface, uint32_t access) {
      VADRMPRIMESurfaceDescriptor desc = {};
      if (vaExportSurfaceHandle(va, surface, VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
                                 access | VA_EXPORT_SURFACE_COMPOSED_LAYERS, &desc) != VA_STATUS_SUCCESS)
        return false;
      bool imported = import(desc);
      for (unsigned i = 0; i < desc.num_objects; ++i) close(desc.objects[i].fd);
      return imported;
    }
    bool load_prime(const AVDRMFrameDescriptor *prime, int width, int height) {
      if (prime->nb_objects > 4 || prime->nb_layers > 4) return false;
      VADRMPRIMESurfaceDescriptor desc = {};
      desc.width = width; desc.height = height;
      desc.num_objects = prime->nb_objects; desc.num_layers = prime->nb_layers;
      for (unsigned i=0; i<desc.num_objects; ++i) {
        desc.objects[i].fd = prime->objects[i].fd;
        desc.objects[i].drm_format_modifier = prime->objects[i].format_modifier;
      }
      for (unsigned i=0; i<desc.num_layers; ++i) {
        auto &layer=prime->layers[i];
        if (layer.nb_planes>4) return false;
        desc.layers[i].drm_format=layer.format;
        desc.layers[i].num_planes=layer.nb_planes;
        for (int j=0;j<layer.nb_planes;++j) {
          desc.layers[i].object_index[j]=layer.planes[j].object_index;
          desc.layers[i].offset[j]=layer.planes[j].offset;
          desc.layers[i].pitch[j]=layer.planes[j].pitch;
        }
      }
      return import(desc);
    }
    // All planes of a scanout live in its one dma-buf.
    bool load_dmabuf(const drmtap_dmabuf_desc &scanout) {
      VADRMPRIMESurfaceDescriptor desc = {};
      desc.width = scanout.width; desc.height = scanout.height;
      desc.num_objects = 1; desc.num_layers = 1;
      desc.objects[0].fd = scanout.dma_buf_fd;
      desc.objects[0].drm_format_modifier = scanout.modifier;
      desc.layers[0].drm_format = scanout.format;
      desc.layers[0].num_planes = scanout.num_planes ? scanout.num_planes : 1;
      if (desc.layers[0].num_planes > 4) return false;
      for (unsigned j=0; j<desc.layers[0].num_planes; ++j) {
        desc.layers[0].object_index[j]=0;
        desc.layers[0].offset[j]=scanout.offsets[j];
        desc.layers[0].pitch[j]=scanout.pitches[j];
      }
      return import(desc);
    }
    bool import(const VADRMPRIMESurfaceDescriptor &desc) {
      bool valid = desc.num_layers == 1 && desc.layers[0].num_planes <= 4;
      std::vector<EGLint> attrs = {EGL_WIDTH, EGLint(desc.width), EGL_HEIGHT, EGLint(desc.height),
                                   EGL_LINUX_DRM_FOURCC_EXT, EGLint(desc.layers[0].drm_format)};
      const EGLint fd_keys[] = {EGL_DMA_BUF_PLANE0_FD_EXT,EGL_DMA_BUF_PLANE1_FD_EXT,EGL_DMA_BUF_PLANE2_FD_EXT,EGL_DMA_BUF_PLANE3_FD_EXT};
      const EGLint offset_keys[] = {EGL_DMA_BUF_PLANE0_OFFSET_EXT,EGL_DMA_BUF_PLANE1_OFFSET_EXT,EGL_DMA_BUF_PLANE2_OFFSET_EXT,EGL_DMA_BUF_PLANE3_OFFSET_EXT};
      const EGLint pitch_keys[] = {EGL_DMA_BUF_PLANE0_PITCH_EXT,EGL_DMA_BUF_PLANE1_PITCH_EXT,EGL_DMA_BUF_PLANE2_PITCH_EXT,EGL_DMA_BUF_PLANE3_PITCH_EXT};
      for (unsigned p = 0; valid && p < desc.layers[0].num_planes; ++p) {
        auto &layer = desc.layers[0];
        if (layer.object_index[p] >= desc.num_objects) { valid = false; break; }
        auto &obj = desc.objects[layer.object_index[p]];
        attrs.insert(attrs.end(), {fd_keys[p],obj.fd, offset_keys[p],EGLint(layer.offset[p]),pitch_keys[p],EGLint(layer.pitch[p])});
        if (obj.drm_format_modifier != DRM_FORMAT_MOD_INVALID)
          attrs.insert(attrs.end(), {EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT+EGLint(p*2),EGLint(obj.drm_format_modifier),
                                     EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT+EGLint(p*2),EGLint(obj.drm_format_modifier>>32)});
      }
      attrs.push_back(EGL_NONE);
      if (valid) image = owner->create_image(owner->display, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT, nullptr, attrs.data());
      if (image == EGL_NO_IMAGE_KHR) return false;
      glGenTextures(1, &texture); glBindTexture(GL_TEXTURE_2D, texture); texture_params();
      owner->bind_image(GL_TEXTURE_2D, image);
      return glGetError() == GL_NO_ERROR;
    }
  };
};

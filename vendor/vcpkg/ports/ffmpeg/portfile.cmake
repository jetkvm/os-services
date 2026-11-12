# FFmpeg with only the hardware encoders that the service opens, as static
# release libraries for the x64-windows-static, x64-linux and arm64-osx
# triplets. The build steps follow vcpkg's own FFmpeg port; the component
# list is this repository's. ../../UPSTREAM.md describes the patches.
set(VCPKG_BUILD_TYPE release)
# The triplets expect debug libraries too; the service links only release ones.
set(VCPKG_POLICY_MISMATCHED_NUMBER_OF_BINARIES enabled)

vcpkg_from_github(
    OUT_SOURCE_PATH SOURCE_PATH
    REPO ffmpeg/ffmpeg
    REF "n${VERSION}"
    SHA512 21bf3fbcdfd2f41ea6edeab40433fecfe362b09ecaa177a652471b54f9357011b4f74dab18245ab1c26f1a473b94205490baecb42289afd2effd603cd0b22b59
    HEAD_REF master
    PATCHES
        0004-dependencies.patch
        0007-fix-lib-naming.patch
        0024-fix-osx-host-c11.patch
        ffmpeg-videotoolbox-low-delay.patch
        ffmpeg-amf-bitrate.patch
        ffmpeg-videotoolbox-bitrate.patch
        ffmpeg-nvenc-bitrate.patch
        ffmpeg-windows-dll-search.patch
        ffmpeg-vaapi-x2bgr10.patch
)

if(SOURCE_PATH MATCHES " ")
    message(FATAL_ERROR "Error: ffmpeg will not build with spaces in the path. Please use a directory with no spaces")
endif()

if(VCPKG_TARGET_ARCHITECTURE STREQUAL "x64")
    vcpkg_find_acquire_program(NASM)
    get_filename_component(NASM_EXE_PATH "${NASM}" DIRECTORY)
    vcpkg_add_to_path("${NASM_EXE_PATH}")
endif()

# The component list. --disable-everything turns off every encoder,
# decoder, parser, filter, muxer and protocol; the lists below enable the
# encoders by name. The other options keep out what configure would build
# or detect on its own: the programs and documentation, the libraries that
# the service does not link, and the system frameworks and libraries that
# the encoders do not need.
set(ffmpeg_options
    --enable-static
    --disable-shared
    --enable-pic
    --disable-debug
    --disable-programs
    --disable-doc
    --disable-htmlpages
    --disable-manpages
    --disable-podpages
    --disable-txtpages
    --disable-large-tests
    --disable-valgrind-backtrace
    --disable-everything
    --enable-avcodec
    --disable-avdevice
    --disable-avfilter
    --disable-avformat
    --disable-swresample
    --disable-swscale
    --disable-network
    --disable-bzlib
    --disable-zlib
    --disable-sdl2
    --disable-vulkan
    --disable-v4l2-m2m
    --disable-appkit
    --disable-audiotoolbox
    --disable-coreimage
    --disable-metal
    --disable-securetransport
)

if(VCPKG_TARGET_IS_WINDOWS)
    # AMF, NVENC and Quick Sync, which takes the Intel Media SDK dispatcher.
    # The dispatcher installs its headers in include/mfx, and FFmpeg
    # includes them without that folder.
    list(APPEND ffmpeg_options
        --target-os=win32
        --toolchain=msvc
        --disable-inline-asm
        --extra-cflags=-DHAVE_UNISTD_H=0
        "--extra-cflags=-I${CURRENT_INSTALLED_DIR}/include/mfx"
        --enable-d3d11va
        --enable-amf
        --enable-encoder=h264_amf
        --enable-encoder=hevc_amf
        --enable-cuda
        --enable-ffnvcodec
        --enable-encoder=h264_nvenc
        --enable-encoder=hevc_nvenc
        --enable-libmfx
        --enable-encoder=h264_qsv
        --enable-encoder=hevc_qsv
    )
elseif(VCPKG_TARGET_IS_LINUX)
    # VAAPI and NVENC. The KMS capture maps DRM frames to VAAPI surfaces
    # with three filters of libavfilter.
    list(APPEND ffmpeg_options
        --target-os=linux
        --enable-pthreads
        --disable-vdpau
        --disable-xlib
        --enable-libdrm
        --enable-vaapi
        --enable-encoder=h264_vaapi
        --enable-encoder=hevc_vaapi
        --enable-cuda
        --enable-ffnvcodec
        --enable-encoder=h264_nvenc
        --enable-encoder=hevc_nvenc
        --enable-avfilter
        --enable-filter=buffer
        --enable-filter=buffersink
        --enable-filter=hwmap
    )
elseif(VCPKG_TARGET_IS_OSX)
    # VideoToolbox only, for Apple Silicon.
    list(APPEND ffmpeg_options
        --disable-autodetect
        --enable-videotoolbox
        --enable-encoder=h264_videotoolbox
        --enable-encoder=hevc_videotoolbox
        --extra-cflags=-arch
        --extra-cflags=arm64
        --extra-ldflags=-arch
        --extra-ldflags=arm64
    )
else()
    message(FATAL_ERROR "This port builds FFmpeg for Windows, Linux and macOS only")
endif()

if(VCPKG_TARGET_ARCHITECTURE STREQUAL "x64")
    list(APPEND ffmpeg_options --arch=x86_64 --enable-asm --enable-x86asm)
elseif(VCPKG_TARGET_ARCHITECTURE STREQUAL "arm64")
    list(APPEND ffmpeg_options --arch=arm64 --enable-asm --disable-x86asm)
else()
    message(FATAL_ERROR "This port builds FFmpeg for x64 and arm64 only")
endif()

list(JOIN ffmpeg_options " " OPTIONS)

vcpkg_cmake_get_vars(cmake_vars_file)
include("${cmake_vars_file}")

string(APPEND VCPKG_COMBINED_C_FLAGS_RELEASE " -I \"${CURRENT_INSTALLED_DIR}/include\"")

## Setup vcpkg toolchain

set(prog_env "")

if(VCPKG_DETECTED_CMAKE_C_COMPILER)
    get_filename_component(CC_path "${VCPKG_DETECTED_CMAKE_C_COMPILER}" DIRECTORY)
    get_filename_component(CC_filename "${VCPKG_DETECTED_CMAKE_C_COMPILER}" NAME)
    set(ENV{CC} "${CC_filename}")
    string(APPEND OPTIONS " --cc=${CC_filename}")
    if(VCPKG_HOST_IS_WINDOWS AND VCPKG_DETECTED_MSVC)
        string(APPEND OPTIONS " --host-cc=${CC_filename}")
    endif()
    list(APPEND prog_env "${CC_path}")
endif()

if(VCPKG_DETECTED_CMAKE_CXX_COMPILER)
    get_filename_component(CXX_path "${VCPKG_DETECTED_CMAKE_CXX_COMPILER}" DIRECTORY)
    get_filename_component(CXX_filename "${VCPKG_DETECTED_CMAKE_CXX_COMPILER}" NAME)
    set(ENV{CXX} "${CXX_filename}")
    string(APPEND OPTIONS " --cxx=${CXX_filename}")
    list(APPEND prog_env "${CXX_path}")
endif()

if(VCPKG_DETECTED_CMAKE_RC_COMPILER)
    get_filename_component(RC_path "${VCPKG_DETECTED_CMAKE_RC_COMPILER}" DIRECTORY)
    get_filename_component(RC_filename "${VCPKG_DETECTED_CMAKE_RC_COMPILER}" NAME)
    set(ENV{WINDRES} "${RC_filename}")
    string(APPEND OPTIONS " --windres=${RC_filename}")
    list(APPEND prog_env "${RC_path}")
endif()

if(VCPKG_DETECTED_CMAKE_LINKER AND VCPKG_TARGET_IS_WINDOWS)
    get_filename_component(LD_path "${VCPKG_DETECTED_CMAKE_LINKER}" DIRECTORY)
    get_filename_component(LD_filename "${VCPKG_DETECTED_CMAKE_LINKER}" NAME)
    set(ENV{LD} "${LD_filename}")
    string(APPEND OPTIONS " --ld=${LD_filename}")
    list(APPEND prog_env "${LD_path}")
endif()

if(VCPKG_DETECTED_CMAKE_NM)
    get_filename_component(NM_path "${VCPKG_DETECTED_CMAKE_NM}" DIRECTORY)
    get_filename_component(NM_filename "${VCPKG_DETECTED_CMAKE_NM}" NAME)
    set(ENV{NM} "${NM_filename}")
    string(APPEND OPTIONS " --nm=${NM_filename}")
    list(APPEND prog_env "${NM_path}")
endif()

if(VCPKG_DETECTED_CMAKE_AR)
    get_filename_component(AR_path "${VCPKG_DETECTED_CMAKE_AR}" DIRECTORY)
    get_filename_component(AR_filename "${VCPKG_DETECTED_CMAKE_AR}" NAME)
    if(AR_filename MATCHES [[^(llvm-)?lib\.exe$]])
        set(ENV{AR} "ar-lib ${AR_filename}")
        string(APPEND OPTIONS " --ar='ar-lib ${AR_filename}'")
    else()
        set(ENV{AR} "${AR_filename}")
        string(APPEND OPTIONS " --ar='${AR_filename}'")
    endif()
    list(APPEND prog_env "${AR_path}")
endif()

if(VCPKG_DETECTED_CMAKE_RANLIB)
    get_filename_component(RANLIB_path "${VCPKG_DETECTED_CMAKE_RANLIB}" DIRECTORY)
    get_filename_component(RANLIB_filename "${VCPKG_DETECTED_CMAKE_RANLIB}" NAME)
    set(ENV{RANLIB} "${RANLIB_filename}")
    string(APPEND OPTIONS " --ranlib=${RANLIB_filename}")
    list(APPEND prog_env "${RANLIB_path}")
endif()

if(VCPKG_DETECTED_CMAKE_STRIP)
    get_filename_component(STRIP_path "${VCPKG_DETECTED_CMAKE_STRIP}" DIRECTORY)
    get_filename_component(STRIP_filename "${VCPKG_DETECTED_CMAKE_STRIP}" NAME)
    set(ENV{STRIP} "${STRIP_filename}")
    string(APPEND OPTIONS " --strip=${STRIP_filename}")
    list(APPEND prog_env "${STRIP_path}")
endif()

if(VCPKG_HOST_IS_WINDOWS)
    vcpkg_acquire_msys(MSYS_ROOT PACKAGES automake)
    set(SHELL "${MSYS_ROOT}/usr/bin/bash.exe")
    vcpkg_execute_required_process(
        COMMAND "${SHELL}" -c "'/usr/bin/automake' --print-lib"
        OUTPUT_VARIABLE automake_lib
        OUTPUT_STRIP_TRAILING_WHITESPACE
        WORKING_DIRECTORY "${MSYS_ROOT}"
        LOGNAME automake-print-lib
    )
    list(APPEND prog_env "${MSYS_ROOT}/usr/bin" "${MSYS_ROOT}${automake_lib}")
else()
    find_program(SHELL bash)
endif()

list(REMOVE_DUPLICATES prog_env)
vcpkg_add_to_path(PREPEND ${prog_env})

file(REMOVE_RECURSE "${CURRENT_BUILDTREES_DIR}/${TARGET_TRIPLET}-rel")

string(APPEND OPTIONS " --enable-cross-compile")

vcpkg_find_acquire_program(PKGCONFIG)
string(APPEND OPTIONS " --pkg-config=\"${PKGCONFIG}\" --pkg-config-flags=--static")

message(STATUS "Building Options: ${OPTIONS}")

# Release build
if(VCPKG_DETECTED_MSVC)
    set(OPTIONS_RELEASE "--enable-optimizations --extra-ldflags=-libpath:\"${CURRENT_INSTALLED_DIR}/lib\"")
else()
    set(OPTIONS_RELEASE "--enable-optimizations --extra-ldflags=-L\"${CURRENT_INSTALLED_DIR}/lib\"")
endif()
message(STATUS "Building Release Options: ${OPTIONS_RELEASE}")
message(STATUS "Building ${PORT} for Release")
file(MAKE_DIRECTORY "${CURRENT_BUILDTREES_DIR}/${TARGET_TRIPLET}-rel")
# We use response files here as the only known way to handle spaces in paths
set(crsp "${CURRENT_BUILDTREES_DIR}/${TARGET_TRIPLET}-rel/cflags.rsp")
string(REGEX REPLACE "-arch [A-Za-z0-9_]+" "" VCPKG_COMBINED_C_FLAGS_RELEASE_SANITIZED "${VCPKG_COMBINED_C_FLAGS_RELEASE}")
file(WRITE "${crsp}" "${VCPKG_COMBINED_C_FLAGS_RELEASE_SANITIZED}")
set(ldrsp "${CURRENT_BUILDTREES_DIR}/${TARGET_TRIPLET}-rel/ldflags.rsp")
string(REGEX REPLACE "-arch [A-Za-z0-9_]+" "" VCPKG_COMBINED_SHARED_LINKER_FLAGS_RELEASE_SANITIZED "${VCPKG_COMBINED_SHARED_LINKER_FLAGS_RELEASE}")
file(WRITE "${ldrsp}" "${VCPKG_COMBINED_SHARED_LINKER_FLAGS_RELEASE_SANITIZED}")
set(ENV{CFLAGS} "@${crsp}")
set(ENV{ASFLAGS} "@${crsp}")
set(ENV{LDFLAGS} "@${ldrsp}")
set(ENV{ARFLAGS} "${VCPKG_COMBINED_STATIC_LINKER_FLAGS_RELEASE}")

set(BUILD_DIR         "${CURRENT_BUILDTREES_DIR}/${TARGET_TRIPLET}-rel")
set(CONFIGURE_OPTIONS "${OPTIONS} ${OPTIONS_RELEASE}")
set(INST_PREFIX       "${CURRENT_PACKAGES_DIR}")

configure_file("${CMAKE_CURRENT_LIST_DIR}/build.sh.in" "${BUILD_DIR}/build.sh" @ONLY)

z_vcpkg_setup_pkgconfig_path(CONFIG RELEASE)

vcpkg_execute_required_process(
    COMMAND "${SHELL}" ./build.sh
    WORKING_DIRECTORY "${BUILD_DIR}"
    LOGNAME "build-${TARGET_TRIPLET}-rel"
    SAVE_LOG_FILES ffbuild/config.log
)

z_vcpkg_restore_pkgconfig_path()

# The service's build scripts link the libraries by name and read no
# pkg-config file.
file(REMOVE_RECURSE "${CURRENT_PACKAGES_DIR}/lib/pkgconfig")

# The service ships FFmpeg under the LGPL 2.1, so a configure option that
# makes it GPL or nonfree fails the build.
file(STRINGS "${CURRENT_BUILDTREES_DIR}/build-${TARGET_TRIPLET}-rel-out.log" LICENSE_STRING REGEX "License: .*" LIMIT_COUNT 1)
if(NOT LICENSE_STRING STREQUAL "License: LGPL version 2.1 or later")
    message(FATAL_ERROR "FFmpeg's configure reported \"${LICENSE_STRING}\", not LGPL 2.1")
endif()
vcpkg_install_copyright(FILE_LIST "${SOURCE_PATH}/COPYING.LGPLv2.1")

# The Linux build. It lints, tests and packages the service on Ubuntu 22.04,
# so that the AppImage runs on glibc 2.35 and newer, and builds FFmpeg with
# vcpkg. It exports dist/ from the `dist` stage.
# `cargo xtask docker` runs it; CI caches the `deps` stage.

FROM ubuntu:22.04 AS toolchain

ARG DEBIAN_FRONTEND=noninteractive
ENV PATH=/root/.cargo/bin:/usr/local/go/bin:${PATH}

# vcpkg builds FFmpeg; the KMS capture links the computer's libva, libdrm
# and EGL, so only their headers come from Ubuntu. meson, ninja, libcap and
# libseccomp build libdrmtap. The update check links OpenSSL, which cargo
# builds from source with perl and make; libclang reads its headers.
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    ca-certificates \
    curl \
    file \
    git \
    libcap-dev \
    libclang-dev \
    libdrm-dev \
    libegl-dev \
    libgles-dev \
    libseccomp-dev \
    libva-dev \
    meson \
    nasm \
    ninja-build \
    pkg-config \
    python3 \
    tar \
    unzip \
    zip \
    && rm -rf /var/lib/apt/lists/*
ENV VCPKG_ROOT=/opt/vcpkg

# rust-toolchain.toml selects the Rust version when cargo first runs.
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --profile minimal --default-toolchain none
RUN curl --proto '=https' --tlsv1.2 -sSfL https://go.dev/dl/go1.24.4.linux-amd64.tar.gz -o /tmp/go.tar.gz \
    && echo "77e5da33bb72aeaef1ba4418b6fe511bc4d041873cbf82e5aa6318740df98717  /tmp/go.tar.gz" | sha256sum --check --quiet \
    && tar -C /usr/local -xzf /tmp/go.tar.gz \
    && rm /tmp/go.tar.gz

WORKDIR /work

# The slow layers come first and copy only what they read, so a change to
# xtask, packaging or the sources does not rebuild them.
FROM toolchain AS deps

# FFmpeg depends only on the vcpkg manifest and ports. These are the vcpkg
# steps of `cargo xtask setup`, which later finds the libraries installed.
COPY vendor/vcpkg ./vendor/vcpkg
RUN baseline=$(sed -n 's/.*"builtin-baseline": *"\([0-9a-f]*\)".*/\1/p' vendor/vcpkg/vcpkg.json) \
    && test -n "$baseline" \
    && git clone --quiet https://github.com/microsoft/vcpkg "$VCPKG_ROOT" \
    && git -C "$VCPKG_ROOT" checkout --quiet --detach "$baseline" \
    && "$VCPKG_ROOT/bootstrap-vcpkg.sh" -disableMetrics \
    && "$VCPKG_ROOT/vcpkg" install --clean-after-build \
        --triplet x64-linux --host-triplet x64-linux \
        --x-manifest-root=/work/vendor/vcpkg --x-install-root="$VCPKG_ROOT/installed"

# The crates build from the manifests alone, with placeholder sources and
# the KMS capture that build.rs compiles. The placeholder's own artifacts are
# removed so cargo cannot take them for a build of the real sources.
COPY rust-toolchain.toml Cargo.toml Cargo.lock build.rs ./
COPY .cargo ./.cargo
COPY src/video/kms/native ./src/video/kms/native
COPY xtask/Cargo.toml ./xtask/
COPY xtask/src/vcpkg_layout.rs ./xtask/src/
RUN mkdir -p src xtask/src \
    && echo 'fn main() {}' > src/main.rs \
    && echo 'fn main() {}' > xtask/src/main.rs
RUN cargo build --release --locked --package ipkvm-os-services --bins --tests \
    && rm -rf target/release/.fingerprint/ipkvm-os-services-* \
        target/release/deps/ipkvm_os_services-* \
        target/release/ipkvm-os-services*

# The rest of `cargo xtask setup`: libdrmtap, whose header the build checks
# against the copy in src/video/kms/native.
COPY packaging ./packaging
RUN packaging/libdrmtap/build.sh --stage packaging/linux/drmtap \
    && rm -rf src xtask/src

FROM deps AS build
# The version a dev release builds; see src/protocol/mod.rs.
ARG IPKVM_OS_SERVICES_VERSION
COPY . .
RUN cargo xtask lint
RUN cargo xtask test
RUN cargo xtask package

FROM scratch AS dist
COPY --from=build /work/dist/ /

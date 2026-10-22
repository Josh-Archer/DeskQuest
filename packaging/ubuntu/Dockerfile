# Build against Ubuntu 24.04 so packages do not require the developer machine's
# newer GTK/glibc. Nothing is uploaded or installed on the build host.
FROM ubuntu:24.04 AS build
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install --no-install-recommends -y \
    build-essential ca-certificates curl cmake pkg-config clang libclang-dev \
    edid-decode libgtk-4-dev libadwaita-1-dev libglib2.0-dev-bin \
    libpipewire-0.3-dev libva-dev rpm xz-utils \
    && rm -rf /var/lib/apt/lists/*
ARG RUST_VERSION=1.98.0
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | \
    sh -s -- -y --profile minimal --default-toolchain "${RUST_VERSION}"
ENV PATH="/root/.cargo/bin:${PATH}" CARGO_BUILD_JOBS=2 CMAKE_POLICY_VERSION_MINIMUM=3.5
RUN cargo install --locked cargo-deb cargo-generate-rpm
WORKDIR /src
COPY . .
ARG SOURCE_SHA
RUN test -n "${SOURCE_SHA}" && \
    scripts/ci/build-ubuntu-packages.sh && \
    mkdir -p /out && \
    cp target/debian/*.deb target/generate-rpm/*.rpm /out/ && \
    printf '%s\n' "${SOURCE_SHA}" > /out/SOURCE_COMMIT && \
    cd /out && sha256sum *.deb *.rpm SOURCE_COMMIT > SHA256SUMS

FROM scratch AS packages
COPY --from=build /out/ /

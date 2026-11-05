# check=skip=InvalidDefaultArgInFrom
# The release build of tn-p2p-allow-sync: the static musl program and nothing else.
#
# release.conf's hook_build runs it twice without the cache and requires the same program
# both times, and a verify with TN_P2P_ALLOW_REBUILD=1 rebuilds a tag with it and compares:
#   docker buildx build --platform linux/amd64 --no-cache -f etc/Dockerfile --target out \
#     --build-arg BUILD_IMAGE=<release.conf's BUILD_IMAGE> --build-arg GIT_SHA=<commit> \
#     --output type=local,dest=<dir> .
# Every input is fixed: the image by digest, the toolchain, the paths, Cargo.lock, and the
# release profile of Cargo.toml (lto, one codegen unit, stripped). No image is published.

# BUILD_IMAGE comes from release.conf, pinned by digest, so the pin lives in one place; it has
# no default on purpose, which the check directive on the first line lets pass without a warning.
ARG BUILD_IMAGE
FROM ${BUILD_IMAGE} AS build

# the exact toolchain the image ships (rustc 1.94.1 for its musl host), so rustup never
# downloads one and rust-toolchain.toml's components are not fetched; move it with the image
ENV RUSTUP_TOOLCHAIN=1.94.1

# fixed paths: the source and the image's CARGO_HOME end up in panic locations in the program
WORKDIR /build

COPY . .

# the commit for the `commit` line of --version, read with option_env!; the program refuses
# to build with anything other than 40 lowercase hex digits, and prints `unknown` without it
ARG GIT_SHA=
ENV TN_P2P_ALLOW_GIT_SHA=${GIT_SHA}

# parallelism only; one codegen unit makes the output independent of it
ARG CARGO_BUILD_JOBS

RUN cargo build --locked --release --target x86_64-unknown-linux-musl

# buildx --output type=local writes the files of this stage, the program alone
FROM scratch AS out
COPY --from=build /build/target/x86_64-unknown-linux-musl/release/tn-p2p-allow-sync /

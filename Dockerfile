# syntax=docker/dockerfile:1

ARG RUST=rust:1.100.0-alpine3.24@sha256:8d0e82abfdf9a38071e07cabc5afba4ddbe077631a80e4e742a760c38beadc33

FROM --platform=$BUILDPLATFORM ${RUST} AS vendored
ARG VERSION=dev
WORKDIR /hubdouble
COPY . .
RUN --mount=type=cache,sharing=locked,target=/usr/local/cargo/registry \
    cargo vendor --locked --versioned-dirs vendor >> .cargo/config.toml \
    && sed "s/@VERSION@/$VERSION/g" notices/BUILD > BUILD \
    && tar -cf /hubdouble-source.tar .

FROM scratch AS source
COPY --from=vendored /hubdouble-source.tar /hubdouble-source.tar

FROM ${RUST} AS build
ARG VERSION=dev
RUN --mount=type=cache,sharing=locked,target=/usr/local/cargo/registry \
    cargo install --locked --features cli cargo-about@0.9.3 --root /about
WORKDIR /src
COPY --from=vendored /hubdouble-source.tar /hubdouble-source.tar
RUN --network=none --mount=type=cache,target=/src/target \
    tar -xf /hubdouble-source.tar \
    && find . -path ./target -prune -o -type f -exec touch {} + \
    && cargo build --release --frozen -p hubdouble \
    && cp target/release/hubdouble /hubdouble \
    && ! readelf -dlW /hubdouble | grep -qE 'NEEDED|program interpreter' \
    && MIMALLOC_VERBOSE=1 /hubdouble help 2>&1 >/dev/null \
    | grep -q "^option 'arena_eager_commit': 0 " \
    && mkdir /licenses \
    && cp LICENSE /licenses/LICENSE \
    && sed "s/@VERSION@/$VERSION/g" notices/SOURCE > /licenses/SOURCE \
    && /about/bin/cargo-about generate --frozen -m crates/hubdouble/Cargo.toml \
    -o /licenses/THIRD-PARTY-NOTICES about.hbs \
    && printf '\n%s\nlibgit2 wildmatch (BSD-style)\n\nUsed by:\n    libgit2-sys\n\n' \
    ================================================================================ \
    >> /licenses/THIRD-PARTY-NOTICES \
    && cat notices/libgit2-wildmatch >> /licenses/THIRD-PARTY-NOTICES \
    && printf '\n%s\nmusl libc (MIT)\n\nUsed by:\n    the C library of the static binary\n\n' \
    ================================================================================ \
    >> /licenses/THIRD-PARTY-NOTICES \
    && cat notices/musl-COPYRIGHT >> /licenses/THIRD-PARTY-NOTICES \
    && printf '\n%s\nRust 1.100.0 standard library (MIT OR Apache-2.0)\n\nUsed by:\n    the standard library of the static binary\n\n' \
    ================================================================================ \
    >> /licenses/THIRD-PARTY-NOTICES \
    && cat notices/rust-std >> /licenses/THIRD-PARTY-NOTICES

FROM scratch
COPY --from=build /hubdouble /hubdouble
COPY --from=build /licenses /licenses
USER 65532:65532
EXPOSE 8000
ENTRYPOINT ["/hubdouble"]

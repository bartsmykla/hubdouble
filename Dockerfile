# syntax=docker/dockerfile:1

FROM rust:1.100.0-alpine3.24@sha256:8d0e82abfdf9a38071e07cabc5afba4ddbe077631a80e4e742a760c38beadc33 AS build
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo install --locked --features cli cargo-about@0.9.3 --root /about
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    find . -path ./target -prune -o -type f -exec touch {} + \
    && cargo build --release --locked -p hubdouble \
    && cp target/release/hubdouble /hubdouble \
    && ! readelf -dlW /hubdouble | grep -qE 'NEEDED|program interpreter' \
    && MIMALLOC_VERBOSE=1 /hubdouble help 2>&1 >/dev/null \
    | grep -q "^option 'arena_eager_commit': 0 " \
    && mkdir /licenses \
    && cp LICENSE /licenses/LICENSE \
    && /about/bin/cargo-about generate --frozen -m crates/hubdouble/Cargo.toml \
    -o /licenses/THIRD-PARTY-NOTICES about.hbs

FROM scratch
COPY --from=build /hubdouble /hubdouble
COPY --from=build /licenses /licenses
USER 65532:65532
EXPOSE 8000
ENTRYPOINT ["/hubdouble"]

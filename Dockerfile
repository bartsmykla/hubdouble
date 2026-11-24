# syntax=docker/dockerfile:1

FROM rust:1.100.0-alpine3.24@sha256:8d0e82abfdf9a38071e07cabc5afba4ddbe077631a80e4e742a760c38beadc33 AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    find . -path ./target -prune -o -type f -exec touch {} + \
    && cargo build --release --locked -p hubdouble \
    && cp target/release/hubdouble /hubdouble \
    && MIMALLOC_VERBOSE=1 /hubdouble help 2>&1 >/dev/null \
    | grep -q "^option 'arena_eager_commit': 0 "

FROM gcr.io/distroless/static-debian13:nonroot@sha256:aafe993665bf6200db38a02c11a9351e782a84b6a45e7f33b6695638c88e3931
COPY --from=build /hubdouble /hubdouble
EXPOSE 8000
ENTRYPOINT ["/hubdouble"]

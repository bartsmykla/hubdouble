Rebuild hubdouble @VERSION@ from this archive

The archive holds the repository tree at the release tag, every crate of
Cargo.lock in vendor/, and a .cargo/config.toml that makes Cargo read the
crates from vendor/. The build needs no network access.

Rebuild the binary in the image that built the release:

    mkdir hubdouble
    tar -xf hubdouble-source.tar -C hubdouble
    cd hubdouble
    docker run --rm -v "$PWD":/src -w /src \
      rust:1.100.0-alpine3.24@sha256:8d0e82abfdf9a38071e07cabc5afba4ddbe077631a80e4e742a760c38beadc33 \
      cargo build --release --frozen -p hubdouble

The binary is target/release/hubdouble, a static executable.

Relink the binary with a changed xdiff (LGPL-2.1-or-later):

1. Change the files in vendor/libgit2-sys-*/libgit2/deps/xdiff/.
2. In vendor/libgit2-sys-*/.cargo-checksum.json, replace the "files"
   object with {}, so that Cargo accepts the changed files.
3. Run the docker command again. Cargo compiles libgit2 with the changed
   xdiff and links a new binary.

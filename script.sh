#!/bin/bash
set -o errexit
set -o pipefail
set -o nounset
set -o xtrace

CARGO="cargo --color=always"
RUST_LOG="warn,server=debug,reeves=debug,actix=info"
export RUST_BACKTRACE=1

if [ "$1" = build ]; then
    shift

    arg=
    if [ "$#" -ge 1 ]; then
        arg="$1"
        shift
    fi
    if [ "$arg" = "" -o "$arg" = "debug" ]; then
        ./script.sh build binary
        ./script.sh build page --dev
    elif [ "$arg" = "release" ]; then
        ./script.sh build binary --release
        ./script.sh build page --profiling
    elif [ "$arg" = "release-with-allcrates" ]; then
        ./script.sh build binary --release --features "all-crates-analysis"
        ./script.sh build page --profiling

    elif [ "$arg" = "binary" ]; then
        $CARGO build --all "$@"

    elif [ "$arg" = "page" ]; then
        cd page
        rm -rf pkg
        wasm-pack build --no-typescript --target web --mode no-install "$1"
        # https://github.com/rustwasm/wasm-pack/issues/811
        rm pkg/package.json pkg/.gitignore
        cp -r --dereference static/* pkg/
        # https://reproducible-builds.org/docs/archives/
        (cd pkg && tar --sort=name \
            --mtime="@0" \
            --owner=0 --group=0 --numeric-owner \
            --format=pax --pax-option=exthdr.name=%d/PaxHeaders/%f,delete=atime,delete=ctime \
            --transform='s#^\./##' \
            -cf ../pkg.tar *)

    else
        echo "invalid build subcommand"
        exit 1
    fi

elif [ "$1" = doc ]; then
    $CARGO doc
    cd page && $CARGO doc #--target wasm32-unknown-unknown

elif [ "$1" = test ]; then
    RUST_LOG=$RUST_LOG $CARGO test --all-targets

elif [ "$1" = run ]; then
    shift
    RUST_LOG=$RUST_LOG /usr/bin/time ./target/debug/reeves "$@"

elif [ "$1" = run-release ]; then
    shift
    RUST_LOG=$RUST_LOG /usr/bin/time ./target/release/reeves "$@"

elif [ "$1" = prep-container ]; then
    shift
    rm -rf container-state
    mkdir container-state
    cd container-state

    # Rust analyzer
    (cd ../rust-analyzer && cargo build --release)
    cp ../rust-analyzer/target/release/rust-analyzer .

    # Rust
    export RUSTUP_HOME=$(pwd)/rust/rustup
    export CARGO_HOME=$(pwd)/rust/cargo
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- --no-modify-path --default-toolchain 1.54.0 --profile minimal -y --quiet
    rustup component add rust-src
    echo '
[source]

[source.crates-io]
replace-with = "mirror"

[source.mirror]
registry = "http://127.0.0.1:8888/git/crates.io-index"
' > $CARGO_HOME/config
    mkdir -p $CARGO_HOME/registry/{cache,index,src}

    # Main FS
    podman pull ubuntu:20.04
    podman rm -f -i reeves-tmp
    podman create --name reeves-tmp ubuntu:20.04 /bin/true
    podman export reeves-tmp > fs.tar
    podman rm -f reeves-tmp
    mkdir crate
    tar -rf fs.tar crate/ rust/ rust-analyzer
    rmdir crate
    gzip fs.tar

    # bwrap
    cp ../bubblewrap-0.5.0/bwrap .
    gzip -k bwrap

elif [ "$1" = vim ]; then
    vim -p notes script.sh Cargo.toml src/main.rs src/lib.rs reeves-types/src/lib.rs page/src/lib.rs page/Cargo.toml src/server.rs

else
    echo invalid command
    exit 1
fi

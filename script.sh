#!/bin/bash
set -o errexit
set -o pipefail
set -o nounset
set -o xtrace

CARGO="cargo --color=always"
RUST_LOG="debug,server=trace,reeves=trace,actix=info,reqwest=debug,tokio_reactor=info"
export REEVES_CONFIG="$(pwd)/reeves_config.toml"
export RUST_BACKTRACE=1

export REEVES_STATIC_TAR_PATH=page/pkg.tar

if [ "$1" = build ]; then
    shift

    arg=
    if [ "$#" -ge 1 ]; then
        arg="$1"
        shift
    fi
    if [ "$arg" = "" ]; then
        ./script.sh build binary
        ./script.sh build page --dev
    elif [ "$arg" = "release" ]; then
        ./script.sh build binary --release
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
    RUST_LOG=$RUST_LOG ./target/debug/reeves "$@"

elif [ "$1" = srv ]; then
    shift
    ./script.sh build
    RUST_LOG=$RUST_LOG ./target/debug/server serve 127.0.0.1 8000

elif [ "$1" = vim ]; then
    vim -p notes script.sh Cargo.toml src/main.rs src/lib.rs reeves-types/src/lib.rs page/src/lib.rs page/Cargo.toml src/bin/server.rs

else
    echo invalid command
    exit 1
fi

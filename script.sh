#!/bin/bash
set -o errexit
set -o pipefail
set -o nounset
set -o xtrace

CARGO="cargo --color=always"
RUST_LOG="warn,server=trace,reeves=trace,actix=info"
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
    ./script.sh build release
    (cd rust-analyzer && cargo build --release)
    rm -rf container-state
    mkdir container-state
    cp rust-analyzer/target/release/rust-analyzer container-state/
    cd container-state
    export RUSTUP_HOME=$(pwd)/rustup
    export CARGO_HOME=$(pwd)/cargo
    export PATH=$PATH:$(pwd)/cargo/bin
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- --no-modify-path --default-toolchain 1.54.0 --profile minimal -y --quiet
    rustup component add rust-src
    echo '
[source]

[source.crates-io]
replace-with = "mirror"

[source.mirror]
registry = "http://localhost:8888/index"
' > $CARGO_HOME/config
    podman pull ubuntu:20.04

    # Apparently this is the best way to update the registry - https://github.com/rust-lang/crater/pull/301/files
    podman run -it --rm --net host \
        -w /work -e RUSTUP_HOME=/work/rustup -e CARGO_HOME=/work/cargo -v $(pwd):/work \
        ubuntu:20.04 /work/cargo/bin/cargo install lazy_static || true
    echo "Ignore the error above if it just complains about 'there is nothing to install'"

elif [ "$1" = crate-analysis ]; then
    shift
    cd container-state
    export RUSTUP_HOME=$(pwd)/rustup
    export CARGO_HOME=$(pwd)/cargo
    export PATH=$PATH:$(pwd)/cargo/bin
    cd ..
    /usr/bin/time python3 containerrun.py

elif [ "$1" = srv ]; then
    shift
    arg=debug
    if [ "$#" -ge 1 ]; then
        arg="$1"
    fi
    ./script.sh build $arg
    RUST_LOG=$RUST_LOG ./target/$arg/server serve 0.0.0.0 8000

elif [ "$1" = vim ]; then
    vim -p notes script.sh Cargo.toml src/main.rs src/lib.rs reeves-types/src/lib.rs page/src/lib.rs page/Cargo.toml src/bin/server.rs

else
    echo invalid command
    exit 1
fi

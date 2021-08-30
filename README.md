# typesearch.rs

Codename: reeves (like "Rust Jeeves")

## Architecture

The typesearch.rs backend uses:

 - sled to store a mapping from crates to functions, and types to crates
 - meilisearch to support free-text search of types

The typesearch.rs frontend uses:

 - yew for rendering the page

## Prerequisites

 - meilisearch running on 127.0.0.1:7700 with no security - download the latest version from [here](https://github.com/meilisearch/MeiliSearch/releases) and run with `./meilisearch` (no arguments)
 - rust analyzer - download the latest version from [here](https://github.com/rust-analyzer/rust-analyzer/releases)
   - tell typesearch.rs how to find it with the `--rust-analyzer` global flag

Optional:

 - if building the web frontend - wasm-pack - `cargo install wasm-pack`
 - if doing large scale analysis (e.g. top100) - a full crates.io mirror with [criner](https://github.com/the-lean-crate/criner)
   - tell typesearch.rs how to find it with the `--criner-db` global flag
 - if doing container analysis - a running instance of [cargo-cacher](https://github.com/ChrisMacNaughton/cargo-cacher) at 127.0.0.1:8888
 - if doing container analysis - podman

## Get up and running

```
$ git clone git@github.com:hadean/typesearch.rs
[...]
$ cd typesearch.rs
$ ./script.sh build release # this will build the frontend and backend
[...]
$ curl -sSL https://static.crates.io/crates/tar/tar-0.4.37.crate | tar -xz
$ ./script.sh run-release analyze-and-save ./tar-0.4.37
[...]
[2021-08-30T18:49:01Z INFO  reeves] loading workspace at path: ./tar-0.4.37
[2021-08-30T18:49:32Z INFO  reeves] Database loaded:     30.35s
[2021-08-30T18:49:34Z INFO  reeves] found crate: "tar" (import name tar)
[2021-08-30T18:49:38Z INFO  reeves] finished analysing functions, inserting 280 function details into db
[2021-08-30T18:49:38Z DEBUG reeves] inserted fndetail: fn Header::new_gnu() -> Header
[2021-08-30T18:49:38Z DEBUG reeves] inserted fndetail: fn Header::new_ustar() -> Header
[2021-08-30T18:49:38Z DEBUG reeves] inserted fndetail: fn Header::new_old() -> Header
[...]
[2021-08-30T18:49:38Z DEBUG reeves] inserted fndetail: fn EntryType::is_pax_local_extensions(&EntryType) -> bool
[2021-08-30T18:49:38Z INFO  reeves] finished inserting into db
$ ./script.sh run-release load-text-search
[...]
[2021-08-30T18:51:47Z WARN  meilisearch_sdk::request] Expected response code 200, got 404
[2021-08-30T18:51:48Z WARN  meilisearch_sdk::request] Expected response code 200, got 404
[2021-08-30T18:51:48Z INFO  reeves] Added 37 entries in total
[2021-08-30T18:51:48Z INFO  reeves] Added 41 entries in total
[...]
$ ./script.sh run-release search 'header' 'u8'
[...]
res: fn Header::as_bytes(&Header) -> &[u8; 512]
res: fn Header::as_bytes(&Header) -> &[u8; 512]
res: fn Header::as_mut_bytes(&mut Header) -> &mut [u8; 512]
res: fn Header::as_mut_bytes(&mut Header) -> &mut [u8; 512]
res: fn Header::path_bytes(&Header) -> Cow<[u8]>
res: fn Header::path_bytes(&Header) -> Cow<[u8]>
res: fn Header::groupname_bytes(&Header) -> Option<&[u8]>
res: fn Header::groupname_bytes(&Header) -> Option<&[u8]>
res: fn Header::username_bytes(&Header) -> Option<&[u8]>
res: fn Header::username_bytes(&Header) -> Option<&[u8]>
[...]
$ ./script.sh run-release serve --port 8000
[...]
[2021-08-30T18:56:46Z INFO  reeves::server] Server starting on 127.0.0.1:8000
[2021-08-30T18:56:46Z INFO  actix_server::builder] Starting 8 workers
[2021-08-30T18:56:46Z INFO  actix_server::builder] Starting "actix-web-service-127.0.0.1:8000" service on 127.0.0.1:8000
```

Visit it in your browser at `http://localhost:8000`!

## TODO

 - Move from sled to sqlite to support multiprocess access
 - Analyse all crates on crates.io

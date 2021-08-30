use reeves;

use isahc::prelude::*;
use log::{debug, info};
use rayon::prelude::*;
use serde::Deserialize;
use std::cmp;
use std::env;
use std::fs;
use std::io::{self, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use structopt::StructOpt;

use reeves_types::*;

// We re-exec this in a container, so need to know how to invoke it
const ANALYZE_AND_PRINT_COMMAND: &str = "analyze-and-print";

// NOTE: this variable assumes that reeves never re-executes itself in the
// same environment (inside a container is fine, as the environment isn't shared)
// We need this because some parts of RA can execute themselves, but we use
// it as a library, so to differentiate whether we're starting reeves or rust
// analyzer, we set this variable on reeves startup
const ENV_RUST_ANALYZER_EXEC: &str = "REEVES_INTERNAL_RUST_ANALYZER_EXEC";
// This gets translated from an argument as soon as reeves starts up, so we know
// what to exec
const ENV_RUST_ANALYZER_BINARY: &str = "REEVES_INTERNAL_RUST_ANALYZER_BINARY";

#[derive(Debug, StructOpt)]
#[structopt(name = "reeves", about = "A tool for indexing and searching crates")]
struct ReevesOpt {
    #[structopt(long, default_value = "reeves.db")]
    db: PathBuf,
    #[structopt(long, default_value = "criner/criner.db")]
    criner_db: PathBuf,
    #[structopt(long, default_value = "rust-analyzer/target/release/rust-analyzer")]
    rust_analyzer: PathBuf,
    #[structopt(subcommand)]
    cmd: ReevesCmd,
}

#[derive(Debug, StructOpt)]
enum ReevesCmd {
    AnalyzeAndSave {
        crate_path: PathBuf,
    },
    #[structopt(name = ANALYZE_AND_PRINT_COMMAND)]
    #[structopt(about = "Analyze a crate and print JSON output (requires: rust analyzer)")]
    AnalyzeAndPrint {
        crate_path: PathBuf,
    },
    #[structopt(about = "Analyze a crate in a secure container and print JSON output (requires: container state)")]
    ContainerAnalyzeAndPrint {
        crate_path: PathBuf,
    },
    #[structopt(about = "Analyze top 100 crates from play.rust-lang.org in containers and save results (requires: container state, criner DB, reeves DB)")]
    AnalyzeTop100Crates,
    #[structopt(about = "Perform a search for some comma-separated param types and a ret type (requires: reeves DB, loaded text search)")]
    Search {
        params_search: String,
        ret_search: String,
    },
    #[structopt(about = "Populate the text search backend, using the reeves DB (requires: reeves DB, running text search)")]
    LoadTextSearch,
    #[structopt(about = "Dump contents of the reeves DB (requires: reeves DB)")]
    DebugDB,
}

fn main() {
    env_logger::init();

    // See comment on ENV_RUST_ANALYZER_EXEC
    if env::var_os(ENV_RUST_ANALYZER_EXEC).is_some() {
        debug!("Re-executing rust-analyzer");
        let mut cmd = Command::new(env::var_os(ENV_RUST_ANALYZER_BINARY).unwrap());
        cmd.args(env::args_os().skip(1)).exec();
        panic!("did not exec");
    } else {
        env::set_var(ENV_RUST_ANALYZER_EXEC, "1");
    }

    let opt = ReevesOpt::from_args();

    env::set_var(ENV_RUST_ANALYZER_BINARY, opt.rust_analyzer);

    match opt.cmd {

        ReevesCmd::AnalyzeAndSave { crate_path } => {
            let (ref krate_name, fndetails) = reeves::analyze_crate_path(&crate_path);
            info!("finished analysing functions, inserting {} function details into db", fndetails.len());
            let db = reeves::open_db(&opt.db);
            reeves::save_analysis(&db, krate_name, fndetails);
            info!("finished inserting into db");
        },

        ReevesCmd::AnalyzeAndPrint { crate_path } => {
            let (krate_name, fndetails) = reeves::analyze_crate_path(&crate_path);
            let out = serde_json::to_vec(&(krate_name, fndetails)).unwrap();
            io::stdout().write_all(&out).unwrap();
        },

        ReevesCmd::ContainerAnalyzeAndPrint { crate_path } => {
            let (krate_name, fndetails) = container_analyze_crate_path(&crate_path);
            let out = serde_json::to_vec(&(krate_name, fndetails)).unwrap();
            io::stdout().write_all(&out).unwrap();
        },

        ReevesCmd::AnalyzeTop100Crates => {
            let criner_db_path = &opt.criner_db;

            #[derive(Deserialize)]
            struct PlayCrates {
                crates: Vec<PlayCrate>,
            }
            #[derive(Deserialize)]
            struct PlayCrate {
                name: String,
                version: String,
                #[allow(unused)]
                id: String, // the alias play uses
            }
            let mut res = isahc::get("https://play.rust-lang.org/meta/crates").unwrap();
            let crates: PlayCrates = res.json().unwrap();

            let db = reeves::open_db(&opt.db);

            let total_crates = crates.crates.len();
            use std::sync::atomic::{AtomicUsize, Ordering};
            let progress = AtomicUsize::new(0);
            fs::create_dir_all("/tmp/crate").unwrap();
            crates.crates.par_iter().for_each(|krate| {
                info!("analyzing crate {}-{}", krate.name, krate.version);
                let crate_tar_path = crate_to_tar_path(criner_db_path.as_ref(), &krate.name, &krate.version);
                let crate_tar_path = crate_tar_path.to_str().unwrap();
                let res = Command::new("tar")
                    .args(&["-C", "/tmp/crate", "-xzf", crate_tar_path])
                    .status().unwrap();
                if !res.success() {
                    panic!("failed to create extracted crate")
                }

                let crate_path = format!("/tmp/crate/{}-{}", krate.name, krate.version);
                let (ref krate_name, fndetails) = container_analyze_crate_path(crate_path.as_ref());
                info!("finished analysing functions for {}, inserting {} function details into db", krate_name, fndetails.len());
                reeves::save_analysis(&db, krate_name, fndetails);
                fs::remove_dir_all(crate_path).unwrap();
                let current_progress = progress.fetch_add(1, Ordering::SeqCst)+1;
                info!("finished inserting into db for {}, completed {}/{} crates", krate_name, current_progress, total_crates);
            });
        }

        ReevesCmd::Search { params_search, ret_search } => {
            let params_search: Vec<_> = if params_search.is_empty() {
                vec![]
            } else {
                params_search.split(",").map(|s| s.trim().to_owned()).collect()
            };
            let ret_search = if ret_search.is_empty() {
                None
            } else {
                Some(ret_search.to_owned())
            };
            let db = reeves::open_db(&opt.db);
            let fndetails = reeves::search(&db, Some(params_search), ret_search);
            for fndetail in fndetails {
                println!("res: {}", fndetail.s)
            }
        }

        ReevesCmd::LoadTextSearch => {
            let db = reeves::open_db(&opt.db);
            reeves::load_text_search(&db)
        },

        ReevesCmd::DebugDB => {
            let db = reeves::open_db(&opt.db);
            reeves::debugdb(&db)
        }

    }
}

fn container_analyze_crate_path(path: &Path) -> (String, Vec<FnDetail>) {
    const OUTPUT_LIMIT: usize = 500;

    let cwd = env::current_dir().unwrap();
    let cwd = cwd.to_str().unwrap();

    // We need to do these so when we actually invoke the crate build scripts etc via rust-analyzer, everything is
    // already downloaded so we can isolate network access
    let res = Command::new("podman").args(&["run", "--rm"])
        // Basics
        .args(&["-v", &format!("{}/container-state:/work", cwd), "-v", &format!("{}:/crate", path.display())])
        .args(&["-e=RUSTUP_HOME=/work/rustup", "-e=CARGO_HOME=/work/cargo"])
        // Custom
        .args(&["-w=/crate", "--net=host"])
        // Command
        .args(&["ubuntu:20.04", "bash", "-c"])
        // TODO: ideally generate-lockfile would use --offline, but it seems to have an issue with a replaced registry
        // when attempting to generate a lockfile for serde-1.0.127
        .arg("/work/cargo/bin/cargo generate-lockfile && /work/cargo/bin/cargo metadata >/dev/null")
        .output().unwrap();

    if !res.status.success() {
        panic!("failed to prep for analysis {}:\n====\n{}\n====\n{}\n====",
               path.display(),
               String::from_utf8_lossy(&res.stdout[..cmp::min(res.stdout.len(), OUTPUT_LIMIT)]),
               String::from_utf8_lossy(&res.stderr[..cmp::min(res.stderr.len(), OUTPUT_LIMIT)])
        )
    }

    let res = Command::new("podman").args(&["run", "--rm"])
        // Basics
        // NOTE: these are read-only
        .args(&["-v", &format!("{}/container-state:/work:ro", cwd), "-v", &format!("{}:/crate:ro", path.display())])
        .args(&["-e=RUSTUP_HOME=/work/rustup", "-e=CARGO_HOME=/work/cargo"])
        // Custom
        .args(&["-w=/work", "--net=none"])
        .args(&["-v", &format!("{}:/reeves:ro", &env::current_exe().unwrap().to_str().unwrap())])
        // Command
        .args(&["ubuntu:20.04", "bash", "-c"])
        .arg(format!("PATH=$PATH:/work/cargo/bin /reeves --rust-analyzer /work/rust-analyzer {} /crate", ANALYZE_AND_PRINT_COMMAND))
        .output().unwrap();

    if !res.status.success() {
        panic!("failed to analyze {}:\n====\n{}\n====\n{}\n====",
               path.display(),
               String::from_utf8_lossy(&res.stdout[..cmp::min(res.stdout.len(), OUTPUT_LIMIT)]),
               String::from_utf8_lossy(&res.stderr[..cmp::min(res.stderr.len(), OUTPUT_LIMIT)])
        )
    }

    match serde_json::from_slice(&res.stdout) {
        Ok(r) => r,
        Err(e) => {
            panic!("failed to deserialize output from analysis in container: {}\n====\n{}\n====",
                   e, String::from_utf8_lossy(&res.stdout[..cmp::min(res.stdout.len(), OUTPUT_LIMIT)]))
        },
    }
}

fn crate_to_tar_path(criner_path: &Path, name: &str, version: &str) -> PathBuf {
    let crate_path = if name.len() >= 4 {
        format!("{}/{}/{}", &name[..2], &name[2..4], name)
    } else if name.len() == 3 {
        format!("3/{}/{}", &name[..1], &name[1..3])
    } else if name.len() == 2 {
        format!("2/{}", &name)
    } else if name.len() == 1 {
        format!("1/{}", &name)
    } else {
        unreachable!("crate name invalid: {:?}", name)
    };

    let version_path = format!("{}-download:1.0.0.crate", version);

    criner_path.join("assets").join(crate_path).join(version_path)
}

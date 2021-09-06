use reeves;

use anyhow::{Context, Result, bail};
use isahc::prelude::*;
use log::{debug, info, warn};
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

mod server;

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

const CRATE_WORK_DIR: &str = "/tmp/crate";

#[derive(Debug, StructOpt)]
#[structopt(name = "reeves", about = "A tool for indexing and searching crates")]
struct ReevesOpt {
    #[structopt(long, default_value = "reeves.db")]
    db: PathBuf,
    #[structopt(long, default_value = "panamax-mirror")]
    panamax_mirror: PathBuf,
    #[structopt(long, default_value = "rust-analyzer/target/release/rust-analyzer")]
    rust_analyzer: PathBuf,
    #[structopt(subcommand)]
    cmd: ReevesCmd,
}

#[derive(Debug, StructOpt)]
enum ReevesCmd {
    #[structopt(about = "Analyze a crate and save results (requires: rust analyzer)")]
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
    #[structopt(about = "Analyze top 100 crates from play.rust-lang.org in containers and save results (requires: container state, panamax mirror, reeves DB)")]
    AnalyzeTop100Crates,
    #[structopt(about = "Analyze all crates (latest version) from crates.io in containers and save results (requires: container state, panamax mirror, reeves DB)")]
    AnalyzeAllCrates,
    #[structopt(about = "Populate the text search backend, using the reeves DB (requires: reeves DB, running text search)")]
    LoadTextSearch,
    #[structopt(about = "Perform a search for some comma-separated param types and a ret type (requires: reeves DB, running+loaded text search)")]
    Search {
        params_search: String,
        ret_search: String,
    },
    #[structopt(about = "Start the reeves server (requires: wasm built, reeves db, loaded+running text search)")]
    Serve {
        #[structopt(long, default_value = "page/pkg.tar")]
        static_tar: PathBuf,
        #[structopt(long, default_value = "127.0.0.1")]
        ip: String,
        #[structopt(long)]
        port: String,
    },
    #[structopt(about = "Dump contents of the reeves DB (requires: reeves DB)")]
    DebugDB,
}

fn main() -> Result<()> {
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
            let (krate_name, krate_version, fndetails) = reeves::analyze_crate_path(&crate_path);
            info!("finished analysing functions, inserting {} function details into db", fndetails.len());
            let db = reeves::open_db(&opt.db);
            reeves::save_analysis(&db, &krate_name, &krate_version, fndetails);
            info!("finished inserting into db");
        },

        ReevesCmd::AnalyzeAndPrint { crate_path } => {
            let (krate_name, krate_version, fndetails) = reeves::analyze_crate_path(&crate_path);
            let out = serde_json::to_vec(&(krate_name, krate_version, fndetails)).unwrap();
            io::stdout().write_all(&out).unwrap();
        },

        ReevesCmd::ContainerAnalyzeAndPrint { crate_path } => {
            let (krate_name, krate_version, fndetails) = container_analyze_crate_path(&crate_path)
                .with_context(|| format!("failed to analyze path {} in a container", crate_path.display()))?;
            let out = serde_json::to_vec(&(krate_name, krate_version, fndetails)).unwrap();
            io::stdout().write_all(&out).unwrap();
        },

        ReevesCmd::AnalyzeTop100Crates => {
            let panamax_mirror_path = &opt.panamax_mirror;

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
            // TODO: stop iteration on panic or report somehow?
            crates.crates.par_iter().for_each(|krate| {
                info!("analyzing crate {}-{}", krate.name, krate.version);
                let fndetails = match container_analyze_crate(panamax_mirror_path, &krate.name, &krate.version) {
                    Ok(fds) => fds,
                    Err(e) => {
                        warn!("failed to analyze {}-{}: {:?}", krate.name, krate.version, e);
                        return
                    }
                };
                info!("finished analysing functions for {} {}, inserting {} function details into db", krate.name, krate.version, fndetails.len());
                reeves::save_analysis(&db, &krate.name, &krate.version, fndetails);
                let current_progress = progress.fetch_add(1, Ordering::SeqCst)+1;
                info!("finished inserting into db for {} {}, completed {}/{} crates", krate.name, krate.version, current_progress, total_crates);
            });
        }

        ReevesCmd::AnalyzeAllCrates => {
            let panamax_mirror_path = &opt.panamax_mirror;

            let db = reeves::open_db(&opt.db);

            let index = crates_index::Index::new(panamax_mirror_path.join("crates.io-index"));
            assert!(index.exists());

            // TODO: exclude yanked versions?
            info!("identifying crates to analyze");
            let crates: Vec<_> = index.crates().map(|c| (c.name().to_owned(), c.highest_version().version().to_owned())).collect();

            info!("looking at {} crates to filter those already in db", crates.len());
            let crates: Vec<_> = crates.into_par_iter().filter(|(name, version)| !reeves::has_crate(&db, name, version)).collect();

            use std::sync::Mutex;
            #[derive(Debug)]
            struct Counter {
                errored: usize,
                processed: usize,
            }
            let count = Mutex::new(Counter { errored: 0, processed: 0 });
            info!("considering {} crates", crates.len());
            // TODO: stop iteration on panic or report somehow?
            crates.par_iter().for_each(|(name, version)| {
                info!("analyzing crate {}-{}", name, version);
                let fndetails = match container_analyze_crate(panamax_mirror_path, name, version) {
                    Ok(fds) => fds,
                    Err(e) => {
                        warn!("failed to analyze {}-{}: {:?}", name, version, e);
                        {
                            let mut count = count.lock().unwrap();
                            count.errored += 1;
                        }
                        return
                    }
                };
                info!("finished analysing functions for {} {}, inserting {} function details into db", name, version, fndetails.len());
                reeves::save_analysis(&db, &name, &version, fndetails);
                info!("finished inserting into db for {} {}", name, version);
                {
                    let mut count = count.lock().unwrap();
                    count.processed += 1;
                    println!("progress: {} processed, {} errored, {} remaining", count.processed, count.errored, crates.len() - (count.processed + count.errored));
                }
            });
            println!("finished: {:?}", count);
        }

        ReevesCmd::LoadTextSearch => {
            let db = reeves::open_db(&opt.db);
            reeves::load_text_search(&db)
        },

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

        ReevesCmd::Serve { ip, port, static_tar } => {
            let db = reeves::open_db(&opt.db);
            let addr = format!("{}:{}", ip, port);
            server::serve(db, addr, static_tar)
        },

        ReevesCmd::DebugDB => {
            let db = reeves::open_db(&opt.db);
            reeves::debugdb(&db)
        }

    }

    Ok(())
}

fn container_analyze_crate(panamax_mirror_path: &Path, krate_name: &str, krate_version: &str) -> Result<Vec<FnDetail>> {
    let crate_tar_path = crate_to_tar_path(panamax_mirror_path, krate_name, krate_version);
    let crate_tar_path = crate_tar_path.to_str().unwrap(); // where the crate tar currently is
    let crate_path = format!("{}/{}-{}", CRATE_WORK_DIR, krate_name, krate_version); // where it will get extracted to

    fs::create_dir_all(CRATE_WORK_DIR).unwrap();
    if let Err(e) = fs::remove_dir_all(&crate_path) {
        if e.kind() != io::ErrorKind::NotFound { panic!("{}", e) }
    }

    let res = Command::new("tar")
        .args(&["-C", CRATE_WORK_DIR, "-xzf", crate_tar_path])
        .status().unwrap();
    if !res.success() {
        bail!("failed to create extracted crate")
    }

    let res = container_analyze_crate_path(crate_path.as_ref());
    fs::remove_dir_all(crate_path).unwrap();

    let (krate_name_, krate_version_, fndetails) = res.context("failed to analyze crate")?;
    assert_eq!((krate_name, krate_version), (krate_name_.as_str(), krate_version_.as_str()));

    Ok(fndetails)
}

fn container_analyze_crate_path(path: &Path) -> Result<(String, String, Vec<FnDetail>)> {
    const OUTPUT_LIMIT: usize = 500;
    fn snip_output(mut s: &[u8]) -> String {
        let mut didsnip = false;
        if s.len() > OUTPUT_LIMIT {
            s = &s[..OUTPUT_LIMIT];
            didsnip = true;
        }
        let mut out = String::from_utf8_lossy(s).into_owned();
        if didsnip {
            out.push_str("[...snipped...]");
        }
        out
    }

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
        bail!("failed to prep for analysis {}:\n====\n{}\n====\n{}\n====", path.display(), snip_output(&res.stdout), snip_output(&res.stderr))
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
        bail!("failed to analyze {}:\n====\n{}\n====\n{}\n====", path.display(), snip_output(&res.stdout), snip_output(&res.stderr))
    }

    match serde_json::from_slice(&res.stdout) {
        Ok(r) => Ok(r),
        Err(e) => {
            bail!("failed to deserialize output from analysis in container: {}\n====\n{}\n====",
                   e, String::from_utf8_lossy(&res.stdout[..cmp::min(res.stdout.len(), OUTPUT_LIMIT)]))
        },
    }
}

fn crate_to_tar_path(panamax_mirror_path: &Path, name: &str, version: &str) -> PathBuf {
    let crate_path = if name.len() >= 4 {
        format!("{}/{}/{}", &name[..2], &name[2..4], name)
    } else if name.len() == 3 {
        format!("3/{}", name)
    } else if name.len() == 2 {
        format!("2/{}", name)
    } else if name.len() == 1 {
        format!("1/{}", name)
    } else {
        unreachable!("crate name invalid: {:?}", name)
    };

    let version_path = format!("{}/{}-{}.crate", version, name, version);

    panamax_mirror_path.join("crates").join(crate_path).join(version_path)
}

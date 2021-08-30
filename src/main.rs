use reeves;

use isahc::prelude::*;
use log::{trace, debug, info};
use rayon::prelude::*;
use serde::Deserialize;
use std::cmp;
use std::env;
use std::fs;
use std::io::{self, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use reeves_types::*;

const ANALYZE_AND_PRINT_COMMAND: &str = "analyze-and-print";

fn criner_db_path_from_env() -> PathBuf {
    env::var_os("REEVES_CRINER_DB").unwrap().into()
}

fn main() {
    env_logger::init();

    let args: Vec<_> = env::args().collect();
    if args[1] != "x" {
        // We need this because some parts of RA can execute themselves
        let mut cmd = match env::var_os("RUST_ANALYZER_BINARY") {
            Some(v) => Command::new(v),
            None => Command::new("./rust-analyzer/target/release/rust-analyzer"),
        };
        cmd.args(env::args_os().skip(1)).exec();
        panic!("did not exec");
    }

    if args[2] == "analyze-and-save" {
        let path: &Path = args[3].as_ref();

        let (ref krate_name, fndetails) = reeves::analyze_crate_path(path);
        info!("finished analysing functions, inserting {} function details into db", fndetails.len());
        let db = reeves::open_db();
        reeves::save_analysis(&db, krate_name, fndetails);
        info!("finished inserting into db");

    } else if args[2] == ANALYZE_AND_PRINT_COMMAND {
        let path: &Path = args[3].as_ref();

        let (krate_name, fndetails) = reeves::analyze_crate_path(path);
        let out = serde_json::to_vec(&(krate_name, fndetails)).unwrap();
        io::stdout().write_all(&out).unwrap();

    } else if args[2] == "container-analyze-and-print" {
        let path: &Path = args[3].as_ref();

        let (krate_name, fndetails) = container_analyze_crate_path(path);
        let out = serde_json::to_vec(&(krate_name, fndetails)).unwrap();
        io::stdout().write_all(&out).unwrap();

    } else if args[2] == "analyze-top100-crates" {
        let criner_db_path = criner_db_path_from_env();

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

        let db = reeves::open_db();

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

    } else if args[2] == "search" {
        let params_search = &args[3];
        let params_search: Vec<_> = if params_search.is_empty() {
            vec![]
        } else {
            params_search.split(",").map(|s| s.trim().to_owned()).collect()
        };
        let ret_search = &args[4];
        let ret_search = if ret_search.is_empty() {
            None
        } else {
            Some(ret_search.to_owned())
        };
        let db = reeves::open_db();
        let fndetails = reeves::search(&db, Some(params_search), ret_search);
        for fndetail in fndetails {
            println!("res: {}", fndetail.s)
        }

    } else if args[2] == "load-text-search" {
        let db = reeves::open_db();
        reeves::load_text_search(&db)

    } else if args[2] == "debugdb" {
        let db = reeves::open_db();
        reeves::debugdb(&db)

    } else {
        panic!("unknown command")
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
        .args(&["-e=RUST_ANALYZER_BINARY=/work/rust-analyzer", "-e=RUSTUP_HOME=/work/rustup", "-e=CARGO_HOME=/work/cargo"])
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
        .args(&["-v", &format!("{}/container-state:/work:ro", cwd), "-v", &format!("{}:/crate:ro", path.display())])
        .args(&["-e=RUST_ANALYZER_BINARY=/work/rust-analyzer", "-e=RUSTUP_HOME=/work/rustup", "-e=CARGO_HOME=/work/cargo"])
        // Custom
        .args(&["-w=/work", "--net=none"])
        .args(&["-v", &format!("{}:/reeves:ro", &env::current_exe().unwrap().to_str().unwrap())])
        // Command
        .args(&["ubuntu:20.04", "bash", "-c"])
        .arg(format!("PATH=$PATH:/work/cargo/bin /reeves x {} /crate", ANALYZE_AND_PRINT_COMMAND))
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

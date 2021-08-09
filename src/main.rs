use reeves;

use std::env;
use std::io::{self, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

fn main() {
    let args: Vec<_> = env::args().collect();
    if args[1] != "x" {
        Command::new("./rust-analyzer/target/release/rust-analyzer")
            .args(env::args_os().skip(1))
            .exec();
        panic!("did not exec");
    }


    if args[2] == "analyze" {
        let path: &Path = args[3].as_ref();
        let name: &str = &args[4];

        let db = reeves::open_db();
        reeves::analyze_and_save(&db, path, name)
    } else if args[2] == "analyze-print" {
        let path: &Path = args[3].as_ref();
        let name: &str = &args[4];

        let fndetails = reeves::analyze(path, name);
        let out = serde_json::to_vec(&fndetails).unwrap();
        io::stdout().write_all(&out).unwrap();
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
    } else if args[2] == "debugdb" {
        let db = reeves::open_db();
        reeves::debugdb(&db)
    }
}

use base_db::Upcast;
use hir::db::{DefDatabase, HirDatabase};
use hir::{HasVisibility, HirDisplay};
use hir::Crate;
use hir::ItemInNs;
use hir::ModuleDef;
use hir::Visibility;
use hir::import_map::ImportInfo;
use profile::StopWatch;
use project_model::CargoConfig;
use rust_analyzer::cli::load_cargo::{LoadCargoConfig, load_workspace_at};

use std::collections::HashSet;
use std::env;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

fn stop_watch() -> StopWatch {
    StopWatch::start()
}

fn main() {
    let args: Vec<_> = env::args().collect();
    if args[1] != "x" {
        Command::new("./rust-analyzer/target/release/rust-analyzer")
            .args(env::args_os().skip(1))
            .exec();
        panic!("did not exec");
    }

    let path: &Path = "./bincode".as_ref();

    let mut cargo_config = CargoConfig::default();
    cargo_config.no_sysroot = false;
    let load_cargo_config = LoadCargoConfig {
        load_out_dirs_from_check: true, // build scripts
        with_proc_macro: true,
        prefill_caches: false,
    };

    analyze(path, cargo_config, load_cargo_config)
}

fn analyze(path: &Path, cargo_config: CargoConfig, load_cargo_config: LoadCargoConfig) {
    let mut db_load_sw = stop_watch();
    let (host, _vfs, _proc_macro) =
        load_workspace_at(&path, &cargo_config, &load_cargo_config, &|_| {}).unwrap();
    let rootdb = host.raw_database();
    eprintln!("{:<20} {}", "Database loaded:", db_load_sw.elapsed());

    let hirdb: &dyn HirDatabase = rootdb.upcast();
    let defdb: &dyn DefDatabase = rootdb.upcast();

    let krates = Crate::all(hirdb);
    for krate in krates {
        if krate.display_name(hirdb).unwrap().to_string() != "bincode" {
            continue
        }
        eprintln!("{:?}", krate.display_name(hirdb));
        eprintln!("");
        let mut moddefs = HashSet::new();
        let import_map = defdb.import_map(krate.into());
        for (item, importinfo) in import_map.map.iter() {
            let item: ItemInNs = item.to_owned().into();
            let moddef = if let Some(moddef) = item.as_module_def() { moddef } else { continue };
            let isnew = moddefs.insert(moddef);
            if !isnew { continue }
            let path = &importinfo.path.to_string();
            match moddef {
                ModuleDef::Function(f) => print_function(hirdb, f, path),
                ModuleDef::Adt(a) => print_adt(hirdb, a, path),
                ModuleDef::Trait(t) => print_trait(hirdb, t, path),
                x @ ModuleDef::Variant(_) |
                x @ ModuleDef::Const(_) |
                x @ ModuleDef::Static(_) |
                x @ ModuleDef::Module(_) |
                x @ ModuleDef::TypeAlias(_) |
                x @ ModuleDef::BuiltinType(_) => {
                    eprintln!("XXX skipping ty {} {:?}", x.name(hirdb).unwrap(), x)
                },
            }
            eprintln!("");
        }
        //for item in krate.query_external_importables(defdb, hir::import_map::Query::new(String::new())) {
        //    let function = match item.left() {
        //        Some(hir::ModuleDef::Function(f)) => f,
        //        _ => continue,
        //    };
        //    eprintln!("{:?} {}", function, function.name(hirdb));
        //}
        eprintln!("finished printing functions");
    }
}

fn print_function(hirdb: &dyn HirDatabase, function: hir::Function, path: &str) {
    let self_param_pretty = function.self_param(hirdb)
        .map(|param| param.display(hirdb).to_string());
    let assoc_params_pretty = function.assoc_fn_params(hirdb)
        .into_iter().map(|param| param.ty().display(hirdb).to_string())
        .collect::<Vec<_>>();
    let params_pretty = function.method_params(hirdb).map(|params| {
        params.into_iter().map(|param| param.ty().display(hirdb).to_string())
            .collect::<Vec<_>>()
    });
    let ret_pretty = function.ret_type(hirdb).display(hirdb).to_string();
    eprintln!("fn {} ({:?} | {:?} | {:?} | {})", path,
        self_param_pretty, assoc_params_pretty, params_pretty, ret_pretty);
}
fn print_adt(hirdb: &dyn HirDatabase, adt: hir::Adt, path: &str) {
    let mut methods = vec![];
    let ty = adt.ty(hirdb);
    let krate = adt.module(hirdb).krate();
    let _: Option<()> = ty.clone().iterate_assoc_items(hirdb, krate, |associtem| {
        if let hir::AssocItem::Function(f) = associtem { methods.push(f) }
        None
    });
    let _: Option<()> = ty.iterate_method_candidates(hirdb, krate, &Default::default(), None, |_ty, f| {
        methods.push(f);
        None
    });
    let methods: Vec<_> = methods.into_iter()
        .filter(|m| m.visibility(hirdb) == Visibility::Public).collect();
    eprintln!("adt {} {:?}", path, methods);
    for method in methods {
        print_function(hirdb, method, &(path.to_owned() + "::" + &method.name(hirdb).to_string()));
    }
}
fn print_trait(hirdb: &dyn HirDatabase, tr: hir::Trait, path: &str) {
    eprintln!("trait {} {:?}", path, tr.items(hirdb));
}



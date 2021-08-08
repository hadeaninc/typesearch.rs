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

use serde::{Serialize, Deserialize};
use sled::Transactional;
use sled::transaction::TransactionError;
use void::Void;

use std::collections::HashSet;
use std::env;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::str;

#[derive(Serialize, Deserialize)]
struct FnDetail {
    params: String,
    ret: String,
    s: String,
}

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

    let db: sled::Db = sled::open("reeves.db").unwrap();

    if args[2] == "analyze" {
        let path: &Path = args[3].as_ref();
        let name: &str = &args[4];

        let mut cargo_config = CargoConfig::default();
        cargo_config.no_sysroot = false;
        let load_cargo_config = LoadCargoConfig {
            load_out_dirs_from_check: true, // build scripts
            with_proc_macro: true,
            prefill_caches: false,
        };

        analyze(&db, path, name, cargo_config, load_cargo_config)
    } else if args[2] == "search" {
        let params_search = &args[3];
        let ret_search = &args[4];
        search(&db, params_search, ret_search)
    } else if args[2] == "debugdb" {
        debugdb(&db)
    }
}

fn analyze(db: &sled::Db, path: &Path, name: &str, cargo_config: CargoConfig, load_cargo_config: LoadCargoConfig) {
    let mut db_load_sw = stop_watch();
    if !path.is_dir() {
        panic!("path is not a directory")
    }
    eprintln!("loading workspace at path: {}", path.display());
    let (host, _vfs, _proc_macro) =
        load_workspace_at(&path, &cargo_config, &load_cargo_config, &|_| {}).unwrap();
    let rootdb = host.raw_database();
    eprintln!("{:<20} {}", "Database loaded:", db_load_sw.elapsed());

    let hirdb: &dyn HirDatabase = rootdb.upcast();
    let defdb: &dyn DefDatabase = rootdb.upcast();

    if !db.contains_key("next_fn_id").unwrap() {
        db.insert("next_fn_id", bincode::serialize(&0u64).unwrap()).unwrap();
    }

    let krates = Crate::all(hirdb);
    for krate in krates {
        let krate_display_name = krate.display_name(hirdb).unwrap();
        if krate_display_name.to_string() != name {
            continue
        }
        eprintln!("{:?}", krate_display_name);
        eprintln!("");
        let mut moddefs = HashSet::new();
        let import_map = defdb.import_map(krate.into());
        let mut fndetails = vec![];
        for (item, importinfo) in import_map.map.iter() {
            let item: ItemInNs = item.to_owned().into();
            let moddef = if let Some(moddef) = item.as_module_def() { moddef } else { continue };
            let isnew = moddefs.insert(moddef);
            if !isnew { continue }
            let path = &importinfo.path.to_string();
            let import_fndetails = match moddef {
                ModuleDef::Function(f) => analyze_function(hirdb, f, path),
                ModuleDef::Adt(a) => analyze_adt(hirdb, a, path),
                ModuleDef::Trait(t) => analyze_trait(hirdb, t, path),
                x @ ModuleDef::Variant(_) |
                x @ ModuleDef::Const(_) |
                x @ ModuleDef::Static(_) |
                x @ ModuleDef::Module(_) |
                x @ ModuleDef::TypeAlias(_) |
                x @ ModuleDef::BuiltinType(_) => {
                    eprintln!("skipping ty {} {:?}", x.name(hirdb).unwrap(), x);
                    vec![]
                },
            };
            fndetails.extend(import_fndetails);
            eprintln!("");
        }
        eprintln!("finished printing functions, inserting {} function details into db", fndetails.len());
        purge_crate(db, &krate_display_name.to_string());
        add_crate(db, &krate_display_name.to_string(), fndetails);
        eprintln!("finished inserting into db");
    }
}

fn search(db: &sled::Db, params_search: &str, ret_search: &str) {
    let params_tree = db.open_tree("params").unwrap();
    let ret_tree = db.open_tree("ret").unwrap();
    let fn_tree = db.open_tree("fn").unwrap();

    let match_fns_params: HashSet<u64> = params_tree.get(params_search).unwrap()
        .map(|ivec| bincode::deserialize(&ivec).unwrap()).unwrap_or_else(HashSet::new);
    let match_fns_ret: HashSet<u64> = ret_tree.get(ret_search).unwrap()
        .map(|ivec| bincode::deserialize(&ivec).unwrap()).unwrap_or_else(HashSet::new);
    for fn_id in match_fns_params.intersection(&match_fns_ret) {
        let fn_bytes = fn_tree.get(fn_id.to_le_bytes()).unwrap().unwrap();
        let fndetail: FnDetail = bincode::deserialize(&fn_bytes).unwrap();
        println!("res: {}", fndetail.s)
    }
}

fn debugdb(db: &sled::Db) {
    fn debugtree(tree: &sled::Tree) {
        for kv in tree.iter() {
            let (key, val) = kv.unwrap();
            let short_val_str = if val.len() > 16 {
                format!("{:?}...", &val[..16])
            } else {
                format!("{:?}", val)
            };
            eprintln!("key: {:?} | {:?} -> {}", String::from_utf8_lossy(&key), key, short_val_str)
        }
    }

    for treename in db.tree_names() {
        eprintln!("# tree: {:?}", String::from_utf8_lossy(&treename));
        let tree = db.open_tree(&treename).unwrap();
        debugtree(&tree);
    }
}

fn add_crate(db: &sled::Db, name: &str, fndetails: Vec<FnDetail>) -> u64 {
    let params_tree = db.open_tree("params").unwrap();
    let ret_tree = db.open_tree("ret").unwrap();
    let fn_tree = db.open_tree("fn").unwrap();
    let crate_tree = db.open_tree("crate").unwrap();
    let ret: Result<u64, TransactionError<Void>> = (&**db, &params_tree, &ret_tree, &fn_tree, &crate_tree)
        .transaction(|(db, params_tree, ret_tree, fn_tree, crate_tree)| {
            let mut fn_id: u64 = bincode::deserialize(&db.get("next_fn_id").unwrap().unwrap()).unwrap();
            let mut fn_ids = vec![];
            for fndetail in fndetails.iter() {
                let mut params_set = params_tree.get(&fndetail.params).unwrap()
                    .map(|d| bincode::deserialize(d.as_ref()).unwrap()).unwrap_or_else(HashSet::new);
                let isnew = params_set.insert(fn_id);
                assert!(isnew);
                params_tree.insert(fndetail.params.as_bytes(), bincode::serialize(&params_set).unwrap()).unwrap();

                let mut ret_set = ret_tree.get(&fndetail.ret).unwrap()
                    .map(|d| bincode::deserialize(d.as_ref()).unwrap()).unwrap_or_else(HashSet::new);
                let isnew = ret_set.insert(fn_id);
                assert!(isnew);
                ret_tree.insert(fndetail.ret.as_bytes(), bincode::serialize(&ret_set).unwrap()).unwrap();

                fn_tree.insert(bincode::serialize(&fn_id).unwrap(), bincode::serialize(fndetail).unwrap()).unwrap();
                fn_ids.push(fn_id);

                eprintln!("inserted: {}", fndetail.s);

                fn_id += 1
            }
            crate_tree.insert(name, bincode::serialize(&fn_ids).unwrap()).unwrap();
            db.insert("next_fn_id", bincode::serialize(&fn_id).unwrap()).unwrap();
            Ok(fn_id)
        });
    ret.unwrap()
}

fn purge_crate(db: &sled::Db, name: &str) {
    let params_tree = db.open_tree("params").unwrap();
    let ret_tree = db.open_tree("ret").unwrap();
    let fn_tree = db.open_tree("fn").unwrap();
    let crate_tree = db.open_tree("crate").unwrap();
    let ret: Result<(), TransactionError<Void>> = (&**db, &params_tree, &ret_tree, &fn_tree, &crate_tree)
        .transaction(|(_db, params_tree, ret_tree, fn_tree, crate_tree)| {
            let fn_ids: Vec<u64> = match crate_tree.remove(name).unwrap() {
                Some(fn_ids) => bincode::deserialize(&fn_ids).unwrap(),
                None => return Ok(()),
            };
            let fndetails: Vec<(u64, FnDetail)> = fn_ids.into_iter()
                .map(|fn_id| (fn_id, fn_tree.remove(bincode::serialize(&fn_id).unwrap()).unwrap().unwrap()))
                .map(|(fn_id, bytes)| (fn_id, bincode::deserialize(&bytes).unwrap()))
                .collect();
            for (fn_id, fndetail) in fndetails {
                let mut params_set: HashSet<u64> = params_tree.get(&fndetail.params).unwrap()
                    .map(|d| bincode::deserialize(d.as_ref()).unwrap()).unwrap_or_else(HashSet::new);
                let didremove = params_set.remove(&fn_id);
                assert!(didremove);
                params_tree.insert(fndetail.params.as_bytes(), bincode::serialize(&params_set).unwrap()).unwrap();

                let mut ret_set: HashSet<u64> = ret_tree.get(&fndetail.ret).unwrap()
                    .map(|d| bincode::deserialize(d.as_ref()).unwrap()).unwrap_or_else(HashSet::new);
                let didremove = ret_set.remove(&fn_id);
                assert!(didremove);
                ret_tree.insert(fndetail.ret.as_bytes(), bincode::serialize(&ret_set).unwrap()).unwrap();
            }
            Ok(())
        });
    let () = ret.unwrap();
}

fn analyze_function(hirdb: &dyn HirDatabase, function: hir::Function, path: &str) -> Vec<FnDetail> {
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
    let s = format!("fn {} ({:?}) -> {}", path, assoc_params_pretty, ret_pretty);
    vec![FnDetail {
        params: format!("{:?}", assoc_params_pretty),
        ret: ret_pretty,
        s,
    }]
}

fn analyze_adt(hirdb: &dyn HirDatabase, adt: hir::Adt, path: &str) -> Vec<FnDetail> {
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
        analyze_function(hirdb, method, &(path.to_owned() + "::" + &method.name(hirdb).to_string()));
    }
    vec![]
}

fn analyze_trait(hirdb: &dyn HirDatabase, tr: hir::Trait, path: &str) -> Vec<FnDetail> {
    eprintln!("trait {} {:?}", path, tr.items(hirdb));
    vec![]
}

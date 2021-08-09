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

use sled::Transactional;
use sled::transaction::TransactionError;
use std::collections::HashSet;
use std::path::Path;
use std::str;
use void::Void;

use reeves_types::*;

fn stop_watch() -> StopWatch {
    StopWatch::start()
}

pub fn open_db() -> sled::Db {
    let db = sled::open("reeves.db").unwrap();
    if !db.contains_key("next_fn_id").unwrap() {
        db.insert("next_fn_id", bincode::serialize(&0u64).unwrap()).unwrap();
    }
    db
}

pub fn analyze_and_save(db: &sled::Db, path: &Path, krate_name: &str) {
    let fndetails = analyze(path, krate_name);
    eprintln!("finished printing functions, inserting {} function details into db", fndetails.len());
    purge_crate(db, krate_name);
    add_crate(db, krate_name, fndetails);
    eprintln!("finished inserting into db");
}

pub fn analyze(path: &Path, krate_name: &str) -> Vec<FnDetail> {
    let mut db_load_sw = stop_watch();
    if !path.is_dir() {
        panic!("path is not a directory")
    }
    eprintln!("loading workspace at path: {}", path.display());
    let mut cargo_config = CargoConfig::default();
    cargo_config.no_sysroot = false;
    let load_cargo_config = LoadCargoConfig {
        load_out_dirs_from_check: false, // build scripts
        with_proc_macro: false,
        prefill_caches: false,
    };
    let (host, _vfs, _proc_macro) =
        load_workspace_at(&path, &cargo_config, &load_cargo_config, &|_| {}).unwrap();
    let rootdb = host.raw_database();
    eprintln!("{:<20} {}", "Database loaded:", db_load_sw.elapsed());

    let hirdb: &dyn HirDatabase = rootdb.upcast();
    let defdb: &dyn DefDatabase = rootdb.upcast();

    let krates = Crate::all(hirdb);
    for krate in krates {
        let canonical_name = krate.display_name(hirdb).unwrap().canonical_name().to_owned();
        if canonical_name != krate_name {
            continue
        }
        eprintln!("found crate: {:?}", krate_name);
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
        return fndetails
    }
    panic!("didn't find crate {}!", krate_name)
}

pub fn search(db: &sled::Db, params_search: Option<Vec<String>>, ret_search: Option<String>) -> Vec<FnDetail> {
    let param_tree = db.open_tree("param").unwrap();
    let ret_tree = db.open_tree("ret").unwrap();
    let fn_tree = db.open_tree("fn").unwrap();

    if params_search.is_none() && ret_search.is_none() {
        return vec![]
    }

    let mut match_fns: Option<HashSet<u64>> = None;

    if let Some(ret_search) = ret_search {
        match_fns = Some(ret_tree.get(ret_search).unwrap()
            .map(|ivec| bincode::deserialize(&ivec).unwrap()).unwrap_or_else(HashSet::new))
    }

    if let Some(mut params_search) = params_search {
        if params_search.is_empty() {
            params_search = vec!["<NOARGS>".into()];
        }
        for param in params_search {
            let match_fns_param: HashSet<u64> = param_tree.get(param).unwrap()
                .map(|ivec| bincode::deserialize(&ivec).unwrap()).unwrap_or_else(HashSet::new);
            let new_match_fns = if let Some(match_fns) = match_fns.take() {
                match_fns.intersection(&match_fns_param).cloned().collect()
            } else {
                match_fns_param
            };
            if new_match_fns.is_empty() {
                return vec![]
            }
            match_fns = Some(new_match_fns)
        }
    }

    let mut ret = vec![];
    for fn_id in match_fns.expect("no match fns, but should have been caught earlier") {
        let fn_bytes = fn_tree.get(bincode::serialize(&fn_id).unwrap()).unwrap().unwrap();
        let fndetail: FnDetail = bincode::deserialize(&fn_bytes).unwrap();
        ret.push(fndetail);
    }

    ret.sort_by(|fd1, fd2| fd1.s.cmp(&fd2.s));

    ret
}

pub fn debugdb(db: &sled::Db) {
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
    let param_tree = db.open_tree("param").unwrap();
    let ret_tree = db.open_tree("ret").unwrap();
    let fn_tree = db.open_tree("fn").unwrap();
    let crate_tree = db.open_tree("crate").unwrap();
    let ret: Result<u64, TransactionError<Void>> = (&**db, &param_tree, &ret_tree, &fn_tree, &crate_tree)
        .transaction(|(db, param_tree, ret_tree, fn_tree, crate_tree)| {
            let mut fn_id: u64 = bincode::deserialize(&db.get("next_fn_id").unwrap().unwrap()).unwrap();
            let mut fn_ids = vec![];
            let nil_params: Vec<String> = vec!["<NOARGS>".into()];
            for fndetail in fndetails.iter() {
                let mut params = &fndetail.params;
                if params.is_empty() {
                    params = &nil_params;
                }
                for param in params.iter() {
                    let mut param_set = param_tree.get(param).unwrap()
                        .map(|d| bincode::deserialize(d.as_ref()).unwrap()).unwrap_or_else(HashSet::new);
                    // May not be new if multiple params of the same type
                    let _isnew = param_set.insert(fn_id);
                    param_tree.insert(param.as_bytes(), bincode::serialize(&param_set).unwrap()).unwrap();
                }

                let mut ret_set = ret_tree.get(&fndetail.ret).unwrap()
                    .map(|d| bincode::deserialize(d.as_ref()).unwrap()).unwrap_or_else(HashSet::new);
                let isnew = ret_set.insert(fn_id);
                assert!(isnew, "{:?}", fndetail.s);
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
    let param_tree = db.open_tree("param").unwrap();
    let ret_tree = db.open_tree("ret").unwrap();
    let fn_tree = db.open_tree("fn").unwrap();
    let crate_tree = db.open_tree("crate").unwrap();
    let ret: Result<(), TransactionError<Void>> = (&**db, &param_tree, &ret_tree, &fn_tree, &crate_tree)
        .transaction(|(_db, param_tree, ret_tree, fn_tree, crate_tree)| {
            let fn_ids: Vec<u64> = match crate_tree.remove(name).unwrap() {
                Some(fn_ids) => bincode::deserialize(&fn_ids).unwrap(),
                None => return Ok(()),
            };
            let fndetails: Vec<(u64, FnDetail)> = fn_ids.into_iter()
                .map(|fn_id| (fn_id, fn_tree.remove(bincode::serialize(&fn_id).unwrap()).unwrap().unwrap()))
                .map(|(fn_id, bytes)| (fn_id, bincode::deserialize(&bytes).unwrap()))
                .collect();
            for (fn_id, fndetail) in fndetails {
                let mut params = fndetail.params;
                if params.is_empty() {
                    params = vec!["<NOARGS>".into()];
                }
                for param in params {
                    let mut param_set: HashSet<u64> = param_tree.get(&param).unwrap()
                        .map(|d| bincode::deserialize(d.as_ref()).unwrap()).unwrap_or_else(HashSet::new);
                    let didremove = param_set.remove(&fn_id);
                    assert!(didremove, "{:?}", fndetail.s);
                    param_tree.insert(param.as_bytes(), bincode::serialize(&param_set).unwrap()).unwrap();
                }

                let mut ret_set: HashSet<u64> = ret_tree.get(&fndetail.ret).unwrap()
                    .map(|d| bincode::deserialize(d.as_ref()).unwrap()).unwrap_or_else(HashSet::new);
                let didremove = ret_set.remove(&fn_id);
                assert!(didremove, "{:?}", fndetail.s);
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
    let assoc_params_str = assoc_params_pretty.join(", ");
    let s = format!("fn {}({}) -> {}", path, assoc_params_str, ret_pretty);
    vec![FnDetail {
        params: assoc_params_pretty,
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
    let mut fndetails = vec![];
    for method in methods {
        fndetails.extend(analyze_function(hirdb, method, &(path.to_owned() + "::" + &method.name(hirdb).to_string())));
    }
    fndetails
}

fn analyze_trait(hirdb: &dyn HirDatabase, tr: hir::Trait, path: &str) -> Vec<FnDetail> {
    eprintln!("trait {} {:?}", path, tr.items(hirdb));
    vec![]
}

use base_db::Upcast;
use hir::db::{DefDatabase, HirDatabase};
use hir::{HasVisibility, HirDisplay};
use hir::Crate;
use hir::ItemInNs;
use hir::ModuleDef;
use hir::Visibility;
use profile::StopWatch;
use project_model::CargoConfig;
use rust_analyzer::cli::load_cargo::{LoadCargoConfig, load_workspace_at};

use meilisearch_sdk as meili;
use serde::{Serialize, Deserialize};
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

pub fn analyze_and_save(db: &sled::Db, path: &Path) {
    let (ref krate_name, fndetails) = analyze(path);
    eprintln!("finished printing functions, inserting {} function details into db", fndetails.len());
    purge_crate(db, krate_name);
    add_crate(db, krate_name, fndetails);
    eprintln!("finished inserting into db");
}

pub fn analyze(path: &Path) -> (String, Vec<FnDetail>) {
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

    let (krate_name, krate_import_name) = discover_crate_import_name(path, &cargo_config);

    let krates = Crate::all(hirdb);
    for krate in krates {
        let display_name = krate.display_name(hirdb).unwrap().to_string();
        if krate_import_name != display_name {
            continue
        }
        eprintln!("found crate: {:?} (import name {})", krate_name, display_name);
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
                ModuleDef::Function(f) => analyze_function(hirdb, &krate_name, f, path),
                ModuleDef::Adt(a) => analyze_adt(hirdb, &krate_name, a, path),
                ModuleDef::Trait(t) => analyze_trait(hirdb, &krate_name, t, path),
                x @ ModuleDef::Variant(_) |
                x @ ModuleDef::Const(_) |
                x @ ModuleDef::Static(_) |
                x @ ModuleDef::Module(_) |
                x @ ModuleDef::TypeAlias(_) |
                x @ ModuleDef::BuiltinType(_) => {
                    eprintln!("skipping ty {:?} {:?}", x.name(hirdb), x);
                    vec![]
                },
            };
            fndetails.extend(import_fndetails);
            eprintln!("");
        }
        return (krate_name, fndetails)
    }
    panic!("didn't find crate {} (import name {})!", krate_name, krate_import_name)
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

#[derive(Serialize, Deserialize, Debug)]
struct TypeInFn {
    id: u64,
    ty: String,
    orig_ty: String,
}

impl meili::document::Document for TypeInFn {
    type UIDType = u64;

    fn get_uid(&self) -> &Self::UIDType {
        &self.id
    }
}

pub fn load_text_search(db: &sled::Db) {
    let param_tree = db.open_tree("param").unwrap();
    let ret_tree = db.open_tree("ret").unwrap();
    let fn_tree = db.open_tree("fn").unwrap();

    fn tokenize_type(s: &str) -> String {
        let mut s = s
            .replace('<', " < ")
            .replace('>', " > ")
            .replace('[', " [ ")
            .replace(']', " ] ")
            .replace('&', " & ");
        loop {
            let news = s.replace("  ", " ");
            if news == s {
                return s
            }
            s = news
        }
    }

    let client = meili::client::Client::new("http://localhost:7700", "no_key");

    futures::executor::block_on(async move {
        client.delete_index_if_exists("param_types").await.unwrap();
        let param_types = client.get_or_create("param_types").await.unwrap();
        param_types.set_settings(&meili::settings::Settings {
            synonyms: None,
            stop_words: Some(vec![]),
            ranking_rules: None,
            attributes_for_faceting: Some(vec![]),
            distinct_attribute: None,
            searchable_attributes: Some(vec!["ty".into()]),
            displayed_attributes: Some(vec!["orig_ty".into()]),
        }).await.unwrap().wait_for_pending_update(None, None).await.unwrap().unwrap();

        async fn do_batch(index: &meili::indexes::Index, batch: &mut Vec<TypeInFn>, total: &mut usize) {
            index.add_documents(batch, Some("id")).await.unwrap()
                .wait_for_pending_update(None, None).await.unwrap().unwrap();
            *total += batch.len();
            eprintln!("Added {} entries in total", total);
            batch.clear();
        }

        let mut total = 0;
        let mut batch = vec![];
        for (i, kv) in param_tree.iter().enumerate() {
            let (key, _val) = kv.unwrap();
            let str_key = str::from_utf8(&key).unwrap();
            let tokenized_key = tokenize_type(str_key);
            batch.push(TypeInFn { id: i as u64, ty: tokenized_key, orig_ty: str_key.to_owned() });
            if batch.len() > 500 {
                do_batch(&param_types, &mut batch, &mut total).await;
            }
        }
        do_batch(&param_types, &mut batch, &mut total).await;
    })
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

fn discover_crate_import_name(path: &Path, cargo_config: &CargoConfig) -> (String, String) {
    // If you want to see some of the complexity here:
    // - md-5 package name is 'md-5', but target name (and import name) is 'md5'
    //
    // We are taking crates from crates.io, so we can assume:
    // - there is only one package (i.e. not a workspace)
    // - there is only one lib
    use project_model::{ProjectManifest, ProjectWorkspace, TargetKind};
    use std::convert::TryInto;
    let p: &_ = path.try_into().unwrap();
    let root = ProjectManifest::discover_single(&p).unwrap();
    let ws = ProjectWorkspace::load(root, cargo_config, &|_| {}).unwrap();
    //eprintln!("{:#?}", ws);
    let cargo = match ws {
        ProjectWorkspace::Cargo { cargo, .. } => cargo,
        _ => panic!("unexpected workspace type"),
    };
    //eprintln!("{:#?}", cargo);
    let members = cargo.packages().map(|pd| &cargo[pd]).filter(|pd| pd.is_member).collect::<Vec<_>>();
    assert_eq!(members.len(), 1, "{:?}", members);
    let lib_targets = members[0].targets.iter().map(|&t| &cargo[t]).filter(|t| t.kind == TargetKind::Lib).collect::<Vec<_>>();
    assert_eq!(lib_targets.len(), 1, "{:?}", lib_targets);
    //eprintln!("{:?} {:?}", members[0].name, lib_targets[0].name);
    (members[0].name.clone(), lib_targets[0].name.replace('-', "_"))
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
                    // May not be deleted if multiple params of the same type
                    let _didremove = param_set.remove(&fn_id);
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

fn analyze_function(hirdb: &dyn HirDatabase, krate_name: &str, function: hir::Function, path: &str) -> Vec<FnDetail> {
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
        krate: krate_name.to_owned(),
        params: assoc_params_pretty,
        ret: ret_pretty,
        s,
    }]
}

fn analyze_adt(hirdb: &dyn HirDatabase, krate_name: &str, adt: hir::Adt, path: &str) -> Vec<FnDetail> {
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
        fndetails.extend(analyze_function(hirdb, krate_name, method, &(path.to_owned() + "::" + &method.name(hirdb).to_string())));
    }
    fndetails
}

fn analyze_trait(hirdb: &dyn HirDatabase, _krate_name: &str, tr: hir::Trait, path: &str) -> Vec<FnDetail> {
    eprintln!("trait {} {:?}", path, tr.items(hirdb));
    vec![]
}

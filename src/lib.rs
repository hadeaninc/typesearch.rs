use ra_base_db::Upcast;
use ra_hir::db::{DefDatabase, HirDatabase};
use ra_hir::{HasVisibility, HirDisplay};
use ra_hir::Crate;
use ra_hir::ItemInNs;
use ra_hir::ModuleDef;
use ra_hir::Visibility;
use ra_paths::{AbsPath, AbsPathBuf};
use ra_profile::StopWatch;
use ra_project_model::{CargoConfig, ProjectManifest, ProjectWorkspace, TargetKind};
use rust_analyzer::cli::load_cargo::{LoadCargoConfig, load_workspace_at};

use log::{trace, debug, info};
use meilisearch_sdk as meili;
use serde::{Serialize, Deserialize};
use sled::Transactional;
use sled::transaction::TransactionError;
use std::cmp;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::str;
use void::Void;

use reeves_types::*;

const FUZZY_SEARCH_LIMIT: usize = 100;
const MAX_RESULTS: usize = 500;

const FN_ID_COUNTER: &str = "next_fn_id";
const PARAM_TREE: &str = "param";
const RET_TREE: &str = "ret";
const FN_TREE: &str = "fn";
const CRATE_TREE: &str = "crate";

// A sentinel to represent functions with no arguments (must not be a possible type)
const NIL_PARAMS: &str = "<NOARGS>";

// For fuzzy searching
const PARAM_TYPES_INDEX: &str = "param_types";
const RET_TYPES_INDEX: &str = "ret_types";

fn stop_watch() -> StopWatch {
    StopWatch::start()
}

pub fn open_db(path: &Path) -> sled::Db {
    let db = sled::open(path).unwrap();
    if !db.contains_key(FN_ID_COUNTER).unwrap() {
        db.insert(FN_ID_COUNTER, bincode::serialize(&0u64).unwrap()).unwrap();
    }
    db
}

pub fn save_analysis(db: &sled::Db, krate_name: &str, krate_version: &str, fndetails: Vec<FnDetail>) {
    purge_crate(db, krate_name);
    add_crate(db, krate_name, krate_version, fndetails);
}

pub fn has_crate(db: &sled::Db, krate_name: &str, krate_version: &str) -> bool {
    let crate_tree = db.open_tree(CRATE_TREE).unwrap();
    let (version, _fn_ids): (String, Vec<u64>) = match crate_tree.get(krate_name).unwrap() {
        Some(bs) => bincode::deserialize(&bs).unwrap(),
        None => return false,
    };
    version == krate_version
}

pub fn analyze_crate_path(path: &Path) -> (String, String, Vec<FnDetail>) {
    let mut db_load_sw = stop_watch();
    if !path.is_dir() {
        panic!("path is not a directory")
    }
    info!("loading workspace at path: {}", path.display());
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
    info!("{:<20} {}", "Database loaded:", db_load_sw.elapsed());

    let hirdb: &dyn HirDatabase = rootdb.upcast();
    let defdb: &dyn DefDatabase = rootdb.upcast();

    use std::convert::TryInto;
    let abspath: AbsPathBuf = path.canonicalize().unwrap().try_into().unwrap();
    let (krate_name, krate_import_name, krate_version) = discover_crate_import_name(&abspath, &cargo_config);

    let krates = Crate::all(hirdb);
    for krate in krates {
        let display_name = krate.display_name(hirdb).unwrap().to_string();
        if krate_import_name != display_name {
            continue
        }
        info!("found crate: {:?} {} (import name {})", krate_name, krate_version, display_name);
        let mut moddefs = HashSet::new();
        let import_map = defdb.import_map(krate.into());
        let mut fndetails = vec![];
        for (item, importinfo) in import_map.map.iter() {
            let item: ItemInNs = item.to_owned().into();
            // skip macros
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
                    trace!("skipping non-function {:?} {:?}", x.name(hirdb), x);
                    vec![]
                },
            };
            trace!("adding {} items", import_fndetails.len());
            fndetails.extend(import_fndetails);
        }
        return (krate_name, krate_version, fndetails)
    }
    panic!("didn't find crate {} (import name {})!", krate_name, krate_import_name)
}

pub fn search(db: &sled::Db, params_search: Option<Vec<String>>, ret_search: Option<String>) -> Vec<FnDetail> {
    let client = meili::client::Client::new("http://localhost:7700", "no_key");
    let param_types_search = client.assume_index(PARAM_TYPES_INDEX);
    let ret_types_search = client.assume_index(RET_TYPES_INDEX);

    let param_tree = db.open_tree(PARAM_TREE).unwrap();
    let ret_tree = db.open_tree(RET_TREE).unwrap();
    let fn_tree = db.open_tree(FN_TREE).unwrap();

    let mut candidate_types: Vec<(&sled::Tree, Vec<String>)> = vec![];

    if let Some(ret_search) = ret_search {
        let ret_candidates = futures::executor::block_on(async {
            ret_types_search.search()
                .with_query(&ret_search)
                .with_limit(FUZZY_SEARCH_LIMIT)
                .execute::<TypeInFnResult>()
                .await
                .unwrap()
        });
        candidate_types.push((&ret_tree, ret_candidates.hits.into_iter().map(|c| c.result.orig_ty).collect()));
    }

    if let Some(mut params_search) = params_search {
        if params_search.is_empty() {
            params_search = vec!["<NOARGS>".into()];
        }
        for param in params_search {
            let param_candidates = futures::executor::block_on(async {
                param_types_search.search()
                    .with_query(&param)
                    .with_limit(FUZZY_SEARCH_LIMIT)
                    .execute::<TypeInFnResult>()
                    .await
                    .unwrap()
            });
            candidate_types.push((&param_tree, param_candidates.hits.into_iter().map(|c| c.result.orig_ty).collect()));
        }
    }

    // TODO: at each pass, reorder to have the most restrictive type candidates first
    // TODO: at each pass, remember the sets we've built so far so we don't recreate and keep
    // removing the fn ids that have been selected
    let max_candidate_depth = candidate_types.iter().map(|(_, ct)| ct.len()).max().unwrap_or(0);
    let mut fn_ids = vec![];
    let mut fn_ids_set = HashSet::new();
    let mut ranges = vec![];
    for i in 1..max_candidate_depth {
        let mut iteration_fn_ids: Option<HashSet<u64>> = None;
        for (tree, ct_column) in candidate_types.iter() {
            let mut ct_column_fn_ids = HashSet::new();
            for ct in &ct_column[..cmp::min(i, ct_column.len())] {
                let match_fns: HashSet<u64> = tree.get(ct).unwrap()
                    .map(|ivec| bincode::deserialize(&ivec).unwrap())
                    .expect("candidate type did not already have an entry in db");
                ct_column_fn_ids.extend(match_fns)
            }
            // Update the fn ids for this iteration, or initialise them (if the first column)
            if let Some(ifnids) = iteration_fn_ids.as_mut() {
                *ifnids = ifnids.intersection(&ct_column_fn_ids).cloned().collect()
            } else {
                iteration_fn_ids = Some(ct_column_fn_ids)
            }
        }

        let ifnids = iteration_fn_ids.expect("unexpectedly ran out of fn ids");
        let new_fn_ids: Vec<_> = ifnids.difference(&fn_ids_set).cloned().collect();
        ranges.push(fn_ids.len()..fn_ids.len()+new_fn_ids.len());
        fn_ids.extend_from_slice(&new_fn_ids);
        fn_ids_set.extend(new_fn_ids);

        if fn_ids.len() >= MAX_RESULTS {
            break
        }
    }
    let end = cmp::min(fn_ids.len(), MAX_RESULTS);
    let fn_ids = &fn_ids[..end];
    if let Some(range) = ranges.pop() {
        ranges.push(range.start..end)
    }

    let mut ret = vec![];
    for fn_id in fn_ids {
        let fn_bytes = fn_tree.get(bincode::serialize(&fn_id).unwrap()).unwrap().unwrap();
        let fndetail: FnDetail = bincode::deserialize(&fn_bytes).unwrap();
        ret.push(fndetail);
    }

    for range in ranges {
        ret[range].sort_by(|fd1, fd2| {
            let krate_cmp = fd1.krate.cmp(&fd2.krate);
            if krate_cmp.is_eq() { fd1.s.cmp(&fd2.s) } else { krate_cmp }
        });
    }

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

#[derive(Serialize, Deserialize)]
struct TypeInFnResult {
    orig_ty: String,
}

pub fn load_text_search(db: &sled::Db) {
    let param_tree = db.open_tree(PARAM_TREE).unwrap();
    let ret_tree = db.open_tree(RET_TREE).unwrap();

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
        let settings = meili::settings::Settings {
            synonyms: None,
            stop_words: Some(vec![]),
            ranking_rules: None,
            attributes_for_faceting: Some(vec![]),
            distinct_attribute: None,
            searchable_attributes: Some(vec!["ty".into()]),
            displayed_attributes: Some(vec!["orig_ty".into()]),
        };
        client.delete_index_if_exists("param_types").await.unwrap();
        let param_types = client.get_or_create("param_types").await.unwrap();
        param_types.set_settings(&settings).await.unwrap().wait_for_pending_update(None, None).await.unwrap().unwrap();
        client.delete_index_if_exists("ret_types").await.unwrap();
        let ret_types = client.get_or_create("ret_types").await.unwrap();
        ret_types.set_settings(&settings).await.unwrap().wait_for_pending_update(None, None).await.unwrap().unwrap();

        async fn do_batch(index: &meili::indexes::Index, batch: &mut Vec<TypeInFn>, total: &mut usize) {
            index.add_documents(batch, Some("id")).await.unwrap()
                .wait_for_pending_update(None, None).await.unwrap().unwrap();
            *total += batch.len();
            info!("Added {} entries in total", total);
            batch.clear();
        }

        let mut total = 0;
        let mut batch = vec![];
        for (i, kv) in param_tree.iter().enumerate() {
            let (key, _val) = kv.unwrap();
            let str_key = str::from_utf8(&key).unwrap();
            let tokenized_key = tokenize_type(str_key);
            batch.push(TypeInFn { id: i as u64, ty: tokenized_key, orig_ty: str_key.to_owned() });
            if batch.len() >= 500 {
                do_batch(&param_types, &mut batch, &mut total).await;
            }
        }
        do_batch(&param_types, &mut batch, &mut total).await;

        let mut total = 0;
        let mut batch = vec![];
        for (i, kv) in ret_tree.iter().enumerate() {
            let (key, _val) = kv.unwrap();
            let str_key = str::from_utf8(&key).unwrap();
            let tokenized_key = tokenize_type(str_key);
            batch.push(TypeInFn { id: i as u64, ty: tokenized_key, orig_ty: str_key.to_owned() });
            if batch.len() >= 500 {
                do_batch(&ret_types, &mut batch, &mut total).await;
            }
        }
        do_batch(&ret_types, &mut batch, &mut total).await;
    })
}

pub fn debugdb(db: &sled::Db) {
    fn debugtree(name: &str, tree: &sled::Tree) {
        for kv in tree.iter() {
            let (key, val) = kv.unwrap();
            let short_val_str = if val.len() > 16 {
                format!("{:?}...", &val[..16])
            } else {
                format!("{:?}", val)
            };
            info!("tree: {}, key: ({:?} | {:?}) -> {}", name, String::from_utf8_lossy(&key), key, short_val_str)
        }
    }

    for treename in db.tree_names() {
        let namestr = str::from_utf8(&treename).unwrap();
        info!("# tree: {:?}", namestr);
        let tree = db.open_tree(&treename).unwrap();
        debugtree(namestr, &tree);
    }
}

fn discover_crate_import_name(path: &AbsPath, cargo_config: &CargoConfig) -> (String, String, String) { // name, import_name, version
    // If you want to see some of the complexity here:
    // - md-5 package name is 'md-5', but target name (and import name) is 'md5'
    //
    // We are taking crates from crates.io, so we can assume:
    // - there is only one package (i.e. not a workspace)
    // - there is only one lib
    let root = ProjectManifest::discover_single(path).unwrap();
    let ws = ProjectWorkspace::load(root, cargo_config, &|_| {}).unwrap();
    let cargo = match ws {
        ProjectWorkspace::Cargo { cargo, .. } => cargo,
        _ => panic!("unexpected workspace type"),
    };
    let members = cargo.packages().map(|pd| &cargo[pd]).filter(|pd| pd.is_member).collect::<Vec<_>>();
    assert_eq!(members.len(), 1, "{:?}", members);
    let version = members[0].version.to_string();
    let lib_targets = members[0].targets.iter().map(|&t| &cargo[t]).filter(|t| t.kind == TargetKind::Lib).collect::<Vec<_>>();
    assert_eq!(lib_targets.len(), 1, "{:?}", lib_targets);
    (members[0].name.clone(), lib_targets[0].name.replace('-', "_"), version)
}

fn add_crate(db: &sled::Db, name: &str, version: &str, fndetails: Vec<FnDetail>) {
    let param_tree = db.open_tree(PARAM_TREE).unwrap();
    let ret_tree = db.open_tree(RET_TREE).unwrap();
    let fn_tree = db.open_tree(FN_TREE).unwrap();
    let crate_tree = db.open_tree(CRATE_TREE).unwrap();

    // Get a guaranteed-unique fn id from the DB. Doesn't matter if it doesn't get used, u64 is
    // pretty big :)
    fn reserve_fn_id_range(db: &sled::Db, num: usize) -> u64 {
        let ret: Result<u64, TransactionError<Void>> = db.transaction(|db| {
            let fn_id: u64 = bincode::deserialize(&db.get(FN_ID_COUNTER).unwrap().unwrap()).unwrap();
            let range_end = fn_id + num as u64;
            db.insert(FN_ID_COUNTER, bincode::serialize(&range_end).unwrap()).unwrap();
            Ok(fn_id)
        });
        ret.unwrap()
    }

    let start_fn_id = reserve_fn_id_range(db, fndetails.len());
    // Calculate everything to update
    let mut param_sets: HashMap<String, HashSet<u64>> = HashMap::new();
    let mut ret_sets: HashMap<String, HashSet<u64>> = HashMap::new();
    let mut fn_ids: Vec<u64> = vec![];
    let nil_params: Vec<String> = vec![NIL_PARAMS.into()];
    for (i, fndetail) in fndetails.iter().enumerate() {
        let fn_id = start_fn_id + i as u64;
        let mut params = &fndetail.params;
        if params.is_empty() {
            params = &nil_params;
        }
        for param in params.iter() {
            let param_set = param_sets.entry(param.to_owned()).or_insert_with(HashSet::new);
            param_set.insert(fn_id);
            // May not be new if multiple params of the same type
            let _isnew = param_set.insert(fn_id);
        }
        let ret_set = ret_sets.entry(fndetail.ret.to_owned()).or_insert_with(HashSet::new);
        let isnew = ret_set.insert(fn_id);
        assert!(isnew, "{:?}", fndetail.s);

        fn_ids.push(fn_id);
    }

    debug!("performed precomputation for crate {} with {} fns", name, fndetails.len());

    let ret: Result<(), TransactionError<Void>> = (&param_tree, &ret_tree, &fn_tree, &crate_tree)
        .transaction(|(param_tree, ret_tree, fn_tree, crate_tree)| {
            debug!("inserting {} params for crate {}", param_sets.len(), name);
            for (param, fn_ids) in param_sets.iter() {
                let mut param_set: HashSet<u64> = param_tree.get(param).unwrap()
                    .map(|d| bincode::deserialize(d.as_ref()).unwrap()).unwrap_or_else(HashSet::new);
                param_set.extend(fn_ids);
                param_tree.insert(param.as_bytes(), bincode::serialize(&param_set).unwrap()).unwrap();
            }

            debug!("inserting {} rets for crate {}", param_sets.len(), name);
            for (ret, fn_ids) in ret_sets.iter() {
                let mut ret_set: HashSet<u64> = ret_tree.get(ret).unwrap()
                    .map(|d| bincode::deserialize(d.as_ref()).unwrap()).unwrap_or_else(HashSet::new);
                ret_set.extend(fn_ids);
                ret_tree.insert(ret.as_bytes(), bincode::serialize(&ret_set).unwrap()).unwrap();
            }

            debug!("inserting {} fndetails for crate {}", fndetails.len(), name);
            for (i, fndetail) in fndetails.iter().enumerate() {
                let fn_id = start_fn_id + i as u64;
                fn_tree.insert(bincode::serialize(&fn_id).unwrap(), bincode::serialize(fndetail).unwrap()).unwrap();
                debug!("inserted fndetail {}/{}: [{}] {}", i+1, fndetails.len(), fndetail.krate, fndetail.s);
            }
            crate_tree.insert(name, bincode::serialize(&(version, &fn_ids)).unwrap()).unwrap();
            Ok(())
        });

    debug!("completed inserting crate {}", name);
    ret.unwrap()
}

fn purge_crate(db: &sled::Db, name: &str) {
    let param_tree = db.open_tree(PARAM_TREE).unwrap();
    let ret_tree = db.open_tree(RET_TREE).unwrap();
    let fn_tree = db.open_tree(FN_TREE).unwrap();
    let crate_tree = db.open_tree(CRATE_TREE).unwrap();
    let ret: Result<(), TransactionError<Void>> = (&**db, &param_tree, &ret_tree, &fn_tree, &crate_tree)
        .transaction(|(_db, param_tree, ret_tree, fn_tree, crate_tree)| {
            let (_version, fn_ids): (String, Vec<u64>) = match crate_tree.remove(name).unwrap() {
                Some(bs) => bincode::deserialize(&bs).unwrap(),
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

fn analyze_function(hirdb: &dyn HirDatabase, krate_name: &str, function: ra_hir::Function, path: &str) -> Vec<FnDetail> {
    let assoc_params_pretty = function.assoc_fn_params(hirdb)
        .into_iter().map(|param| param.ty().display(hirdb).to_string())
        .collect::<Vec<_>>();
    let ret_pretty = function.ret_type(hirdb).display(hirdb).to_string();
    if log::log_enabled!(log::Level::Info) {
        let self_param_pretty = function.self_param(hirdb)
            .map(|param| param.display(hirdb).to_string());
        let params_pretty = function.method_params(hirdb).map(|params| {
            params.into_iter().map(|param| param.ty().display(hirdb).to_string())
                .collect::<Vec<_>>()
        });
        trace!("fn {} ({:?} | {:?} | {:?} | {})", path,
            self_param_pretty, assoc_params_pretty, params_pretty, ret_pretty);
    }
    let assoc_params_str = assoc_params_pretty.join(", ");
    let s = format!("fn {}({}) -> {}", path, assoc_params_str, ret_pretty);
    vec![FnDetail {
        krate: krate_name.to_owned(),
        params: assoc_params_pretty,
        ret: ret_pretty,
        s,
    }]
}

fn analyze_adt(hirdb: &dyn HirDatabase, krate_name: &str, adt: ra_hir::Adt, path: &str) -> Vec<FnDetail> {
    let mut methods = vec![];
    let ty = adt.ty(hirdb);
    let krate = adt.module(hirdb).krate();
    let _: Option<()> = ty.clone().iterate_assoc_items(hirdb, krate, |associtem| {
        if let ra_hir::AssocItem::Function(f) = associtem { methods.push(f) }
        None
    });
    let _: Option<()> = ty.iterate_method_candidates(hirdb, krate, &Default::default(), None, |_ty, f| {
        methods.push(f);
        None
    });
    let methods: Vec<_> = methods.into_iter()
        .filter(|m| m.visibility(hirdb) == Visibility::Public).collect();
    trace!("adt {} {:?}", path, methods);
    let mut fndetails = vec![];
    for method in methods {
        fndetails.extend(analyze_function(hirdb, krate_name, method, &(path.to_owned() + "::" + &method.name(hirdb).to_string())));
    }
    fndetails
}

fn analyze_trait(hirdb: &dyn HirDatabase, _krate_name: &str, tr: ra_hir::Trait, path: &str) -> Vec<FnDetail> {
    trace!("trait {} {:?}", path, tr.items(hirdb));
    vec![]
}

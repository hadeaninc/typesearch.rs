#![recursion_limit="2048"]

#[macro_use]
extern crate log;

extern crate reeves_types;

use std::cmp;
use std::collections::BTreeMap;
use std::f64;
use std::rc::Rc;
use std::sync::Mutex;
use void::Void;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use yew::prelude::*;
use yew::format::{Binary, Nothing};
use yew::services::fetch::{FetchService, FetchTask, Request, Response};

use reeves_types::*;

#[wasm_bindgen]
pub fn main() {
    wasm_logger::init(wasm_logger::Config::new(log::Level::Debug));

    info!("Initializing yew...");
    yew::initialize();

    info!("Creating app...");
    let app: App<ReevesComponent> = App::new();

    let document = web_sys::window().expect("failed to retreieve window").document().expect("failed to retrieve document from window");
    let elt = document.query_selector("#reeves").expect("Error in document query").expect("Failed to find app mount");
    let env = app.mount(elt);
    info!("Mounted app...");

    yew::run_loop();
}

fn ifnode(c: bool, cb: impl FnOnce() -> Html) -> Html {
    if c { cb() } else { nilnode() }
}
fn maybenode<T>(v: Option<T>, cb: impl FnOnce(T) -> Html) -> Html {
    v.map_or_else(nilnode, cb)
}
fn nilnode() -> Html {
    yew::virtual_dom::vnode::VNode::from(yew::virtual_dom::vlist::VList::new())
}

fn href<M>(e: yew::events::MouseEvent, msg: M) -> M {
    e.prevent_default();
    msg
}

fn error_div(e: &str) -> Html {
    html!{ <div class="error">{ format!("ERROR: {}", e) }</div> }
}

#[wasm_bindgen(inline_js = r#"
export function get_base_fetch_path(has_dirty_issues) {
    return window.location.pathname.replace(RegExp("^\\/$"), "");
}
"#)]
extern "C" {
    fn get_base_fetch_path() -> String;
}

struct ReevesApi {
    base_fetch_path: String,
    fetch: FetchService,
    fetches: Rc<Mutex<BTreeMap<u64, FetchTask>>>, // arbitrary id -> request callback
    next_fetch_id: u64,
}

impl ReevesApi {
    fn new(base_fetch_path: String) -> Self {
        Self {
            base_fetch_path,
            fetch: FetchService::new(),
            fetches: Rc::new(Mutex::new(BTreeMap::new())),
            next_fetch_id: 0,
        }
    }

    fn post_search(&mut self, cb: Callback<ReevesMsg>, search_request: proto::SearchRequest) {
        let request = Request::post(format!("{}/reeves/search", self.base_fetch_path))
            .header("Content-Type", "application/octet-stream")
            .body(Ok(bincode::serialize(&search_request).unwrap()))
            .expect("failed to build request");

        let fetch_id = self.next_fetch_id;
        self.next_fetch_id += 1;
        let fetches = self.fetches.clone();
        let handler = move |response: Response<Binary>| {
            assert!(fetches.lock().expect("fetch lock fail for remove").remove(&fetch_id).is_some());
            let (meta, body) = response.into_parts();
            cb.emit(if meta.status.is_success() {
                let body = body.expect("no body present for success");
                let res = bincode::deserialize(&body).expect("success body invalid bincode");
                ReevesMsg::SearchResult(res)
            } else {
                match body {
                    Ok(body) => {
                        let err = String::from_utf8(body).expect("fail body invalid utf8");
                        ReevesMsg::Error(err)
                    },
                    Err(e) => {
                        ReevesMsg::Error(format!("error on fetch: {} (body error: {})", meta.status, e))
                    }
                }
            })
        };
        let task = self.fetch.fetch_binary(request, handler.into()).unwrap();
        assert!(self.fetches.lock().expect("fetch lock fail for insert").insert(fetch_id, task).is_none());
    }
}

pub enum ReevesMsg {
    SearchRequest,
    SearchResult(proto::SearchResult),

    ParamsChange(String),
    RetChange(String),

    Error(String),
}

pub struct ReevesComponent {
    // State from server
    search_results: Vec<FnDetail>,

    // User state
    params: String,
    ret: String,

    // Maintained state
    last_error: Option<String>,

    // Internal guts
    api: ReevesApi,
    msg_callback: Callback<ReevesMsg>,
    link: ComponentLink<Self>,
}

impl Component for ReevesComponent {
    type Message = ReevesMsg;
    type Properties = ();

    fn create(_: Self::Properties, link: ComponentLink<Self>) -> Self {
        let base_fetch_path = get_base_fetch_path();
        let api = ReevesApi::new(base_fetch_path);

        let ret = Self {
            search_results: vec![],

            params: String::new(),
            ret: String::new(),

            last_error: None,

            api,
            msg_callback: link.callback(|msg| msg),
            link,
        };

        ret
    }

    fn update(&mut self, msg: Self::Message) -> ShouldRender {
        match msg {
            ReevesMsg::SearchRequest => {
                info!("Doing search for {:?} {:?}", self.params, self.ret);

                let sr = proto::SearchRequest { params: self.params.clone(), ret: self.ret.clone() };
                self.api.post_search(self.msg_callback.clone(), sr);

                false
            },
            ReevesMsg::SearchResult(sr) => {
                info!("Loaded {} search results", sr.fndetails.len());

                self.search_results = sr.fndetails;

                true
            },

            ReevesMsg::ParamsChange(val) => {
                self.params = val;
                true
            },
            ReevesMsg::RetChange(val) => {
                self.ret = val;
                true
            },

            ReevesMsg::Error(e) => {
                error!("Nooo: {}", e);
                self.last_error = Some(e);

                true
            },
        }
    }

    fn change(&mut self, (): Self::Properties) -> ShouldRender {
        false
    }

    fn view(&self) -> Html {
        macro_rules! cb { ($x:expr) => { self.link.callback($x) } }

        html!{ <>
            <div id="control-pane">
                <header>
                    { "Reeves by Hadean" }
                </header>
                { maybenode(self.last_error.as_ref().map(String::as_str), error_div) }
                { "Params:" }<input oninput=cb!(|data: InputData| ReevesMsg::ParamsChange(data.value))>{ &self.params }</input>
                { "Ret:" }<input oninput=cb!(|data: InputData| ReevesMsg::RetChange(data.value))>{ &self.ret }</input>
                <button onclick=cb!(|_| ReevesMsg::SearchRequest)>{ "Search" }</button>
            </div>
            <div id="results-pane">
                {
                    for self.search_results.iter().map(|fndetail| {
                        html!{
                            <div><code>{ &fndetail.s }</code></div>
                        }
                    })
                }
            </div>
        </> }
    }
}

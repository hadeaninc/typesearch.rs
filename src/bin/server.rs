#[macro_use]
extern crate log;

use actix_web::{App, HttpResponse, HttpServer, Responder};
use actix_web::http::header::{ContentEncoding, ContentType};
use actix_web::middleware;
use actix_web::web;
use filesystem::{FakeFileSystem, FileSystem};
use std::env;
use std::fs;
use std::io::{self, BufReader, Read};
use std::rc::Rc;
use std::sync::Arc;

use reeves_types::*;

macro_rules! resp {
    ($status:ident, $mime:expr, $resp:expr) => {{
        let mime: ContentType = $mime;
        return HttpResponse::$status().set(mime).body($resp)
    }}
}
macro_rules! resp_uncompressed {
    ($status:ident, $mime:expr, $resp:expr) => {{
        use actix_web::dev::BodyEncoding;
        let mime: ContentType = $mime;
        return HttpResponse::$status().set(mime).encoding(ContentEncoding::Identity).body($resp)
    }}
}
macro_rules! respbin {
    ($resp:expr) => {
        resp!(Ok, ContentType::octet_stream(), bincode::serialize($resp).unwrap())
    };
}
macro_rules! respbinerr {
    ($status:ident, $msg:expr) => {{
        let resp = ErrorResponse { err: $msg.to_string() };
        resp!($status, mime!(Application/OctetStream), bincode::serialize(&resp).unwrap())
    }};
}

macro_rules! getbody {
    ($req:expr) => {{
        let mut bodybuf = vec![];
        $req.body.by_ref().take(REQ_SIZE_CAP as u64).read_to_end(&mut bodybuf).unwrap();
        if bodybuf.len() == REQ_SIZE_CAP {
            respbinerr!(BadRequest, "request too large")
        }

        match bincode::deserialize(&bodybuf) {
            Ok(r) => r,
            Err(_) => respbinerr!(BadRequest, "invalid bincode"),
        }
    }};
}


struct InnerData {
    db: sled::Db,
}

impl InnerData {
    fn new(db: sled::Db) -> Self {
        Self { db }
    }
}

#[derive(Clone)]
struct MyServerData {
    s: Arc<InnerData>,
}

type ServerData = web::Data<MyServerData>;

// Handlers

async fn srv_post_reeves_search(state: ServerData, body: web::Bytes) -> impl Responder {
    let proto::SearchRequest { params, ret } = bincode::deserialize(&body).unwrap();
    let fndetails = reeves::search(&state.s.db, &params, &ret);
    let ret = proto::SearchResult {
        fndetails,
    };
    respbin!(&ret)
}

fn load_static() -> FakeFileSystem {
    let rdr = BufReader::new(fs::File::open(env!("REEVES_STATIC_TAR_PATH")).unwrap());
    let ar = tar::Archive::new(rdr);
    archive_to_fake_filesystem(ar)
}

fn archive_to_fake_filesystem(mut ar: tar::Archive<impl Read>) -> FakeFileSystem {
    let filesystem = FakeFileSystem::new();
    for entry in ar.entries().unwrap().into_iter() {
        let mut entry = entry.unwrap();
        let path = entry.path().unwrap().into_owned();
        let entry_type = entry.header().entry_type();
        trace!("considering file type {:?} at {} from tar", entry_type, path.display());
        match entry_type {
            tar::EntryType::Regular => {
                let mut data = Vec::with_capacity(entry.header().size().unwrap() as usize);
                entry.read_to_end(&mut data).unwrap();
                filesystem.create_file(path, data).unwrap();
            },
            tar::EntryType::Directory => {
                filesystem.create_dir(path).unwrap();
            },
            ft => panic!("{} in tar is {:?}", path.display(), ft),
        }
    }
    filesystem
}

// Main control functions

pub fn servemain(args: &[&str]) {
    env_logger::init();

    assert!(args.len() == 2 || args.len() == 1);

    let db = reeves::open_db();

    let addr = if args.len() == 1 {
        let port = args[0];
        format!("0.0.0.0:{}", port)
    } else {
        let ip = args[0];
        let port = args[1];
        format!("{}:{}", ip, port)
    };

    let state = MyServerData { s: Arc::new(InnerData::new(db)) };

    let fake_fs = load_static();

    let app_factory = move || {
        let app = App::new();
        let app = app.data(state.clone());
        let app = app.wrap(middleware::Logger::default());
        let app = app.wrap(middleware::Compress::new(ContentEncoding::Auto));
        let app = app.route("/reeves/search", web::post().to(srv_post_reeves_search));
        let app = app.service(actix_files::Files::new_with_filesystem_and_namedfile_open_and_renderer(
            fake_fs.clone(),
            |fs, path| {
                let ret = fs.read_file(path).and_then(|data| {
                    let metadata = actix_files::NamedFileMetadata {
                        modified: None,
                        len: data.len() as u64,
                        ino: None,
                    };
                    actix_files::NamedFile::from_readseek(io::Cursor::new(data), path, metadata)
                });
                trace!("got namedfile request for {} -> {:?}", path.display(), ret.is_ok());
                ret
            },
            Rc::new(|_, _, _| { panic!() }),
            "/",
            "".into(),
        ).index_file("index.html"));
        app
    };

    info!("Server starting on {}", addr);
    actix_rt::System::new("actix server").block_on(async {
        HttpServer::new(app_factory)
            .bind(addr)
            .unwrap()
            .run()
            .await
    }).unwrap()
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let op = &*args[1];
    let args = args[2..].iter().map(String::as_str).collect::<Vec<&str>>();
    let args = args.as_slice();
    match op {
        "serve" => servemain(args),
        _ => panic!(),
    }
}

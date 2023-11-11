use crate::database::User;
use crate::database::{DT, OBJ};
use bson::oid::ObjectId;
use bson::DateTime;
use cfg_if::cfg_if;
use rkyv::{Deserialize, Serialize};
use thiserror::Error;

#[derive(
    rkyv::Archive,
    rkyv::Deserialize,
    rkyv::Serialize,
    Debug,
    PartialEq,
    Clone,
    serde::Serialize,
    serde::Deserialize,
)]
#[archive(compare(PartialEq), check_bytes)]
#[archive_attr(derive(Debug))]
pub struct ServerMsgImg {
    #[with(OBJ)]
    pub _id: ObjectId,
    pub user: User,
    pub user_id: String,
    pub msg_id: String,
    pub org_hash: String,
    pub format: String,
    pub width: u32,
    pub height: u32,
    pub has_high: bool,
    pub has_medium: bool,
    pub has_low: bool,

    #[with(DT)]
    pub modified_at: bson::datetime::DateTime,

    #[with(DT)]
    pub created_at: bson::datetime::DateTime,
}

#[derive(rkyv::Archive, Deserialize, Serialize, Debug, PartialEq)]
#[archive(compare(PartialEq), check_bytes)]
#[archive_attr(derive(Debug))]
pub enum ServerMsg {
    Imgs(Vec<ServerMsgImg>),
    None,
    Reset,
}

pub const SERVER_MSG_IMGS_NAME: &str = "imgs";
pub const SERVER_MSG_RESET_NAME: &str = "reset";
pub const SERVER_MSG_NONE_NAME: &str = "NONE";

impl ServerMsg {
    pub fn name(&self) -> String {
        match self {
            ServerMsg::Imgs(_a) => String::from(SERVER_MSG_IMGS_NAME),
            ServerMsg::Reset => String::from(SERVER_MSG_RESET_NAME),
            ServerMsg::None => String::from(SERVER_MSG_NONE_NAME),
        }
    }
}

#[derive(Error, Debug)]
pub enum WebSerializeError {
    #[error("Invalid bytes, error: {0}")]
    InvalidBytes(String),
}

impl ServerMsg {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, WebSerializeError> {
        let server_msg: Self = rkyv::check_archived_root::<Self>(bytes)
            .or_else(|e| {
                Err(WebSerializeError::InvalidBytes(format!(
                    "Received invalid binary msg: {}",
                    e
                )))
            })?
            .deserialize(&mut rkyv::Infallible)
            .or_else(|e| {
                Err(WebSerializeError::InvalidBytes(format!(
                    "Received invalid binary msg: {:?}",
                    e
                )))
            })?;

        Ok(server_msg)
    }
}

#[derive(rkyv::Archive, Deserialize, Serialize, Debug, PartialEq, Clone)]
#[archive(compare(PartialEq), check_bytes)]
#[archive_attr(derive(Debug))]
pub enum ClientMsg {
    GalleryInit {
        amount: u32,

        #[with(DT)]
        from: DateTime,
    },
}

impl ClientMsg {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, WebSerializeError> {
        let server_msg: Self = rkyv::check_archived_root::<Self>(bytes)
            .or_else(|e| {
                Err(WebSerializeError::InvalidBytes(format!(
                    "check_archived_root failed: {}",
                    e
                )))
            })?
            .deserialize(&mut rkyv::Infallible)
            .or_else(|e| {
                Err(WebSerializeError::InvalidBytes(format!(
                    "deserialize failed: {:?}",
                    e
                )))
            })?;

        Ok(server_msg)
    }
}

cfg_if! {
if #[cfg(feature = "ssr")] {
use actix_web::web::Bytes;
use futures::TryStreamExt;
use mongodb::bson::{doc};
use std::collections::HashMap;
use actix::{Actor, Addr, AsyncContext, Handler, Recipient, StreamHandler};
use actix_files::Files;
use actix_web::{web, App, Error, HttpRequest, HttpResponse, HttpServer};
use actix_web_actors::ws::{self, CloseCode, CloseReason, ProtocolError};
use leptos::get_configuration;
use leptos_actix::{generate_route_list, LeptosRoutes};
use serenity::prelude::*;
use actix_web::dev::Server;
use std::{num::ParseIntError, sync::Arc};

struct MyWs {
    id: uuid::Uuid,
    server_state: ServerState
}


impl MyWs {
    pub async fn gallery_handler(db: crate::database::DB, amount: u32, from: DateTime) -> Result<ServerMsg, ServerMsgError> {
        let  pipeline = vec![
            doc! { "$sort": doc! { "created_at": -1 } },
            doc! { "$match": doc! { "created_at": { "$lt": from } } },
            doc! { "$limit": Some( amount.clamp(25, 10000) as i64) },
            doc! { "$lookup": doc! { "from": "user", "localField": "user_id", "foreignField": "id", "as": "user"} },
            doc! { "$unwind": "$user" }
        ];

        let mut imgs = db.collection_img.aggregate(pipeline, None).await?;

        let mut send_this: Vec<ServerMsgImg> = Vec::new();

        while let Some(result) = imgs.try_next().await? {
            let doc: ServerMsgImg = mongodb::bson::from_document(result)?;
            send_this.push(doc);
        };

        Ok(ServerMsg::Imgs(send_this))
    }
}

impl Actor for MyWs {
    type Context = ws::WebsocketContext<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        println!("started what? {}", self.id);

        let sessions = self.server_state.sessions.try_lock();
        let Ok(mut sessions) = sessions else {
            let error = sessions.err().unwrap();
            println!("Locking WS sessions error: {}", error);
            ctx.close(Some(CloseReason::from(CloseCode::Error)));
            return;
        };
        sessions.insert(self.id, ctx.address());
    }
}

struct VecActor(pub Vec<u8>);

impl actix::Message for VecActor {
    type Result = ();
}


struct ByteActor(pub Bytes);

impl actix::Message for ByteActor {
    type Result = ();
}

impl Handler<VecActor> for MyWs {
    type Result = ();

    fn handle(&mut self, msg: VecActor, ctx: &mut Self::Context) -> Self::Result {
        ctx.binary(msg.0);
    }
}

impl Handler<ByteActor> for MyWs {
    type Result = ();

    fn handle(&mut self, msg: ByteActor, ctx: &mut Self::Context) -> () {
        let db = self.server_state.db.clone();
        let recipient: Recipient<_> = ctx.address().recipient();
        let fut = async move {
            let client_msg = ClientMsg::from_bytes(&msg.0.to_vec());
            let Ok(client_msg) = client_msg else {
                println!("Failed to convert bytes to client msg: {}", client_msg.err().unwrap());
                return;
            };
            let server_msg: Result<ServerMsg, ServerMsgError> = match client_msg {
                ClientMsg::GalleryInit { amount, from} => {
                    MyWs::gallery_handler(db, amount, from).await
                }
            };

            let bytes = match server_msg {
                Ok(server_msg) => rkyv::to_bytes::<_, 256>(&server_msg),
                Err(server_msg_error) => {
                    println!("Failed to create server msg: {}", server_msg_error);
                    rkyv::to_bytes::<_, 256>(&ServerMsg::Reset)
                }
            };

            let Ok(bytes) = bytes else {
                println!("Failed to serialize serevr msg: {}", bytes.err().unwrap());
                return;
            };

            recipient.do_send(VecActor(bytes.into_vec()));
        };
        let fut = actix::fut::wrap_future::<_, Self>(fut);
        let _a = ctx.spawn(fut);
    }
}

impl StreamHandler<Result<ws::Message, ws::ProtocolError>> for MyWs {

    fn handle(&mut self, msg: Result<ws::Message, ProtocolError>, ctx: &mut Self::Context) {
        match msg {
            Ok(ws::Message::Ping(msg)) => {
                ctx.pong(&msg)
            }
            Ok(ws::Message::Text(_text)) => {
            }
            Ok(ws::Message::Binary(bytes)) => {
                ctx.address().do_send(ByteActor(bytes));
            },
            Ok(ws::Message::Close(reason)) => {
                ctx.close(reason)
            }
            Err(e) => {
                println!("ERROR: {:?}", e);
            }
            _ => {
                println!("BOOOM");
            }
        }
    }
}

impl Actor for ServerState {
    type Context = actix::Context<Self>;
}

async fn index(
    req: HttpRequest,
    stream: web::Payload,
    server_state: actix_web::web::Data<ServerState>
) -> Result<HttpResponse, Error> {

    let resp = ws::start(
        MyWs {
            id: uuid::Uuid::new_v4(),
            server_state: server_state.get_ref().to_owned().clone()
        },
        &req,
        stream,
    );

    let ip = req.peer_addr().unwrap().ip();
    let port = req.peer_addr().unwrap().port();

    println!("{:?}:{} {:?}", ip, port, resp);

    resp
}

pub async fn favicon() -> actix_web::Result<actix_files::NamedFile> {
    Ok(actix_files::NamedFile::open(format!(
        "assets/favicon.ico"
    ))?)
}

#[derive(Clone)]
pub struct ServerState {
    sessions: Arc<Mutex<HashMap<uuid::Uuid,Addr<MyWs>>>>,
    db: crate::database::DB,
}

pub async fn create_server(db: crate::database::DB) -> Server {
    let conf = get_configuration(None).await.unwrap();
    let addr = conf.leptos_options.site_addr;
    let routes = generate_route_list(crate::app::App);
    println!("listening on http://{}", &addr);

    let sessions = Arc::new(Mutex::new(HashMap::<uuid::Uuid, Addr<MyWs>>::new()));


    HttpServer::new(move || {
        let leptos_options = &conf.leptos_options;
        let site_root = &leptos_options.site_root;

        App::new()
            .app_data(web::Data::new(ServerState {
                sessions: sessions.clone(),
                db: db.clone(),
            }))
            .route("/favicon.ico", web::get().to(favicon))
            .route("/ws/", web::get().to(index))
            .route("/api/{tail:.*}", leptos_actix::handle_server_fns())
            .service(Files::new("/pkg", format!("{site_root}/pkg")))
            .service(Files::new("/assets", site_root))

            .leptos_routes(
                leptos_options.to_owned(),
                routes.to_owned(),
                crate::app::App,
            )
    })
    .workers(2)
    .bind(&addr)
    .unwrap()
    .run()
}

#[derive(Error, Debug)]
pub enum ServerMsgError {
    #[error("Casting error: {0}.")]
    Cast(#[from] ParseIntError),

    #[error("Mongodb: {0}.")]
    Mongo(#[from] mongodb::error::Error),

    #[error("Bson: {0}.")]
    Bson(#[from] mongodb::bson::de::Error),
}

}
}

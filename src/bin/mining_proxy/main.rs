mod version {
    include!(concat!(env!("OUT_DIR"), "/version.rs"));
}

use broadcaster::BroadcastChannel;
use mining_proxy::client::FEE;
use std::sync::Arc;
use tokio::sync::RwLock;

use tracing_subscriber::{self, fmt::time::FormatTime};

use dotenv::dotenv;
use jsonwebtoken::{decode, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fs::OpenOptions, io::Read};
extern crate openssl_probe;

use actix_web::{dev::ServiceRequest, web, App, Error, HttpServer};

use mining_proxy::{
    client::{encry::accept_en_tcp, tcp::accept_tcp, tls::accept_tcp_with_tls},
    state::Worker,
    util::{config::Settings, logger},
    web::{handles::auth::Claims, AppState, OnlineWorker},
};

use actix_web_static_files;
use anyhow::{bail, Result};
use bytes::BytesMut;
use clap::{crate_version, ArgMatches};
use human_panic::setup_panic;
use native_tls::Identity;

use tokio::{
    fs::File,
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    select,
    sync::mpsc::{self, UnboundedReceiver},
};

include!(concat!(env!("OUT_DIR"), "/generated.rs"));

fn main() -> Result<()> {
    setup_panic!();
    openssl_probe::init_ssl_cert_env_vars();
    dotenv().ok();
    logger::init();
    mining_proxy::init();

    let matches = mining_proxy::util::get_app_command_matches()?;
    if !matches.is_present("server") {
        tracing::info!(
            "版本: {} commit: {} {}",
            crate_version!(),
            version::commit_date(),
            version::short_sha(),
        );

        actix_web::rt::System::with_tokio_rt(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .worker_threads(1)
                .thread_name("main-tokio")
                .build()
                .unwrap()
        })
        .block_on(async_main(matches))?;
    } else {
        //tokio::runtime::start
        tokio_main(&matches)?;
    }
    Ok(())
}

async fn async_main(_matches: ArgMatches<'_>) -> Result<()> {
    let data: AppState = Arc::new(std::sync::Mutex::new(HashMap::new()));

    match OpenOptions::new()
        .write(true)
        .read(true)
        //.create(true)
        //.truncate(true)
        .open("configs.yaml")
    {
        Ok(mut f) => {
            //let configs:Vec<Settings> = vec![];
            let mut configs = String::new();
            if let Ok(len) = f.read_to_string(&mut configs) {
                if len > 0 {
                    let configs: Vec<Settings> =
                        match serde_yaml::from_str(&configs) {
                            Ok(s) => s,
                            Err(e) => {
                                tracing::error!("{}", e);
                                vec![]
                            }
                        };
                    for config in configs {
                        match mining_proxy::util::run_server(&config) {
                            Ok(child) => {
                                let online = OnlineWorker {
                                    child,
                                    config: config.clone(),
                                    workers: vec![],
                                    online: 0,
                                };

                                data.lock()
                                    .unwrap()
                                    .insert(config.name, online);
                            }
                            Err(e) => {
                                tracing::error!("{}", e);
                            }
                        }
                    }
                }
            }
        }
        Err(_) => {}
    };

    let tcp_data = data.clone();
    tokio::spawn(async move { recv_from_child(tcp_data).await });
    let port: i32 = match std::env::var("MINING_PROXY_WEB_PORT") {
        Ok(p) => p.parse().unwrap(),
        Err(_) => 8888,
    };

    let http_data = data.clone();
    let web_sever = if let Ok(http) = HttpServer::new(move || {
        let generated = generate();
        let generated1 = generate();
        use actix_web_grants::GrantsMiddleware;
        let auth = GrantsMiddleware::with_extractor(extract);
        App::new()
            .wrap(auth)
            .app_data(web::Data::new(http_data.clone()))
            .service(
                web::scope("/api")
                    .service(mining_proxy::web::handles::user::login)
                    .service(mining_proxy::web::handles::user::info)
                    .service(mining_proxy::web::handles::user::logout)
                    .service(mining_proxy::web::handles::server::crate_app)
                    .service(mining_proxy::web::handles::server::server_list)
                    .service(mining_proxy::web::handles::server::server)
                    .service(mining_proxy::web::handles::server::dashboard),
            )
            .service(actix_web_static_files::ResourceFiles::new(
                "/", generated1,
            ))
            .service(actix_web_static_files::ResourceFiles::new("", generated))
    })
    .workers(1)
    .bind(format!("0.0.0.0:{}", port))
    {
        http.run()
    } else {
        let mut proxy_server = data.lock().unwrap();

        for (_, other_server) in &mut *proxy_server {
            other_server.child.kill().await?;
        }

        bail!("web端口 {} 被占用了", port);
    };

    tracing::info!("界面启动成功地址为: {}", format!("0.0.0.0:{}", port));
    web_sever.await?;
    Ok(())
}

fn tokio_main(matches: &ArgMatches<'_>) -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async { tokio_run(matches).await })?;

    Ok(())
}

async fn tokio_run(matches: &ArgMatches<'_>) -> Result<()> {
    let config_file_name = matches.value_of("config").unwrap_or("default.yaml");
    let config = Settings::new(config_file_name, true)?;

    match config.check() {
        Ok(_) => {}
        Err(err) => {
            tracing::error!("config配置错误 {}", err);
            std::process::exit(1);
        }
    };

    let p12 = match File::open(config.p12_path.clone()).await {
        Ok(f) => Some(f),
        Err(_) => None,
    };

    let mode = if config.share == 0 {
        "纯代理模式"
    } else if config.share == 1 {
        "抽水模式"
    } else {
        "统一钱包模式"
    };

    let cert;
    tracing::info!("名称 {} 当前启动模式为: {}", config.name, mode);
    let der = include_bytes!("identity.p12");
    if let Some(mut p12) = p12 {
        let mut buffer = BytesMut::with_capacity(10240);
        let read_key_len = p12.read_buf(&mut buffer).await?;
        cert = Identity::from_pkcs12(
            &buffer[0..read_key_len],
            config.p12_pass.clone().as_str(),
        )?;
    } else {
        cert = Identity::from_pkcs12(der, "mypass")?;
    }
    let worker_name = config.share_name.clone();

    let (proxy_lines, proxy_w) =
        mining_proxy::client::proxy_pool_login(&config, worker_name.clone())
            .await?;

    //let (worker_tx, worker_rx) = mpsc::unbounded_channel::<Worker>();
    let mut chan: BroadcastChannel<Vec<String>> = BroadcastChannel::new();

    //let (worker_tx, worker_rx) = mpsc::unbounded_channel::<Worker>();
    let (job_send, job_recv) = async_channel::bounded::<Vec<String>>(1);

    let mconfig = Arc::new(RwLock::new(config));

    let proxy = Arc::new(mining_proxy::proxy::Proxy {
        config: mconfig,
        chan,
        job_recv,
        job_send,
        proxy_write: Arc::new(tokio::sync::Mutex::new(proxy_w)),
    });

    let res = tokio::try_join!(
        accept_tcp(Arc::clone(&proxy)),
        accept_en_tcp(Arc::clone(&proxy)),
        // accept_tcp_with_tls(
        //     cert
        // ),
        //send_to_parent(worker_rx, &config),
        mining_proxy::client::fee::fee(
            Arc::clone(&proxy),
            proxy_lines,
            worker_name.clone(),
        ),
    );

    if let Err(err) = res {
        tracing::error!("致命错误 : {}", err);
    }

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendToParentStruct {
    name: String,
    worker: Worker,
}

async fn send_to_parent(
    mut worker_rx: UnboundedReceiver<Worker>, config: &Settings,
) -> Result<()> {
    loop {
        if let Ok(mut stream) =
            tokio::net::TcpStream::connect("127.0.0.1:65500").await
        {
            //let name = config.name.clone();
            loop {
                select! {
                    Some(w) = worker_rx.recv() => {
                        let send = SendToParentStruct{
                            name:config.name.clone(),
                            worker:w,
                        };
                        let mut rpc = serde_json::to_vec(&send)?;
                        rpc.push(b'\n');
                        stream.write(&rpc).await.unwrap();
                    },
                }
            }
        } else {
            tracing::error!("无法链接到主控web端");
            tokio::time::sleep(tokio::time::Duration::from_secs(60 * 2)).await;
        }
    }
}

async fn recv_from_child(app: AppState) -> Result<()> {
    let address = "127.0.0.1:65500";
    let listener = match tokio::net::TcpListener::bind(address.clone()).await {
        Ok(listener) => listener,
        Err(_) => {
            tracing::info!("本地端口被占用 {}", address);
            std::process::exit(1);
        }
    };

    tracing::info!("本地TCP端口{} 启动成功!!!", &address);
    loop {
        let (mut stream, _) = listener.accept().await?;
        let inner_app = app.clone();

        tokio::spawn(async move {
            let (r, _) = stream.split();
            let r_buf = BufReader::new(r);
            let mut r_lines = r_buf.lines();

            loop {
                if let Ok(Some(buf_str)) = r_lines.next_line().await {
                    if let Ok(online_work) =
                        serde_json::from_str::<SendToParentStruct>(&buf_str)
                    {
                        #[cfg(debug_assertions)]
                        dbg!("{}", &online_work);

                        if let Some(temp_app) =
                            inner_app.lock().unwrap().get_mut(&online_work.name)
                        {
                            let mut is_update = false;
                            for worker in &mut temp_app.workers {
                                if worker.worker == online_work.worker.worker {
                                    //dbg!(&worker);
                                    *worker = online_work.worker.clone();
                                    is_update = true;
                                }
                            }
                            if is_update == false {
                                temp_app.workers.push(online_work.worker);
                            }
                        } else {
                            tracing::error!("未找到此端口");
                        }
                    }
                };
            }
        });
    }
}

use mining_proxy::JWT_SECRET;

const ROLE_ADMIN: &str = "ROLE_ADMIN";
// You can use both &ServiceRequest and &mut ServiceRequest
async fn extract(req: &mut ServiceRequest) -> Result<Vec<String>, Error> {
    // Here is a place for your code to get user permissions/grants/permissions
    // from a request For example from a token or database
    // tracing::info!("check the Role");
    // println!("{:?}", req.headers().get("token"));

    if req.path() != "/api/user/login" {
        // 判断权限
        if let Some(token) = req.headers().get("token") {
            let token_data = decode::<Claims>(
                token.to_str().unwrap(),
                &DecodingKey::from_secret(JWT_SECRET.as_bytes()),
                &Validation::default(),
            );
            if let Ok(_) = token_data {
                Ok(vec![ROLE_ADMIN.to_string()])
            } else {
                Ok(vec![])
            }
        } else {
            Ok(vec![])
        }
    } else {
        Ok(vec![])
    }
}

#![warn(clippy::all, rust_2018_idioms)]
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // hide console window on Windows in release

mod app;

use std::{
    ops::ControlFlow,
    sync::{mpsc::Receiver, Arc, Mutex},
};

#[cfg(not(target_arch = "wasm32"))]
use tokio;
#[cfg(target_arch = "wasm32")]
use tokio_with_wasm::alias as tokio;


use anyhow::anyhow;
use app::UiUpdate;
use ewebsock::{WsEvent, WsMessage, WsSender};
use futures::{SinkExt, StreamExt};
use log::{error, info};
use ractor::{ActorRef, ActorStatus, call_t};
use ractor_wormhole::{
    conduit::{self, ConduitError, ConduitMessage, ConduitSink, ConduitSource},
    nexus::{start_nexus, NexusActorMessage},
    portal::{Portal, PortalActorMessage},
    util::{ActorRef_Ask, FnActor},
};
use shared::ChatClientMessage;

use tokio::sync::mpsc::UnboundedReceiver;

use futures::{
    Sink,
    task::{Context, Poll},
};
use std::pin::Pin; // Add this if you want logging

// Define a struct to wrap WsSender and implement Sink
struct WsSenderSink {
    sender: ActorRef<ConduitMessage>,
}

impl Sink<ConduitMessage> for WsSenderSink {
    type Error = ConduitError;

    fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if self.sender.get_status() != ActorStatus::Running {
            Poll::Ready(Ok(()))
        } else {
            Poll::Ready(Err(anyhow!("Disconnected")))
        }
    }

    fn start_send(self: Pin<&mut Self>, item: ConduitMessage) -> Result<(), Self::Error> {
        self.sender.send_message(item);
        Ok(())
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.sender
            .send_message(ConduitMessage::Close(Some("Closing the Sink".to_string())));
        Poll::Ready(Ok(()))
    }
}

// note: WsSender is `struct { tx: Option<std::sync::mpsc::Sender<WsMessage>>, }`
// note: ConduitSink is `Pin<Box<dyn Sink<ConduitMessage, Error = ConduitError> + Send>>`
pub async fn adapt_WsSender_to_Conduit(sender: WsSender) -> Result<ConduitSink, ConduitError> {
    // 'WsSender' isn't Send, so create an actor here and wrap it.
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

    let (actor_ref, _handle) = FnActor::start_fn(async move |mut ctx| {
        let mut sender = sender;
        while let Some(msg) = ctx.rx.recv().await {
            match msg {
                ConduitMessage::Text(text) => sender.send(WsMessage::Text(text.to_string())),
                ConduitMessage::Binary(data) => sender.send(WsMessage::Binary(data.to_vec())),
                ConduitMessage::Close(close_frame) => {
                    info!("Closing the WebSocket connection: {:?}", close_frame);
                    sender.close();
                }
            }
        }
    })
    .await?;

    // Create and box the sink implementation
    let sink = WsSenderSink { sender: actor_ref };
    Ok(Box::pin(sink))
}

// note: ConduitSource is `Pin<Box<dyn Stream<Item = Result<ConduitMessage, ConduitError>> + Send>>`
pub fn adapt_tokio_receiver_to_Conduit(rx: UnboundedReceiver<ConduitMessage>) -> ConduitSource {
    // Create a stream from the receiver
    let stream = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|msg| (Ok(msg), rx))
    });

    // Box and pin the stream
    Box::pin(stream)
}

pub async fn connect_to_server(
    nexus: ActorRef<NexusActorMessage>,
    url: String,
) -> Result<ActorRef<PortalActorMessage>, anyhow::Error> {
    info!("Connecting to WebSocket server at: {}", url);

    let (opened_tx, opened_rx) = tokio::sync::oneshot::channel();
    let (_internal_tx, ws_rx) = tokio::sync::mpsc::unbounded_channel();
    let opened_tx = Arc::new(Mutex::new(Some(opened_tx)));
    let handler: Box<dyn Send + Fn(WsEvent) -> ControlFlow<()>> = Box::new(move |evt| {
        match evt {
            WsEvent::Opened => {
                if let Some(opened_tx) = opened_tx.lock().unwrap().take() {
                    let _ = opened_tx.send(());
                };
                info!("WebSocket connection opened");
            }
            WsEvent::Message(ws_message) => {
                match ws_message {
                    WsMessage::Text(text) => {
                        _internal_tx.send(ConduitMessage::Text(text.to_string()));
                    }
                    WsMessage::Binary(bin) => {
                        _internal_tx.send(ConduitMessage::Binary(bin));
                    }
                    _ => {}
                };
            }
            WsEvent::Error(_) => {
                error!("WebSocket error");
            }
            WsEvent::Closed => {
                info!("WebSocket connection closed");
            }
        }
        ControlFlow::Continue(())
    });
    let ws_tx = ewebsock::ws_connect(url.clone(), ewebsock::Options::default(), handler)
        .map_err(|err| anyhow!(err))?;

    // Wait for the connection to be opened
    opened_rx.await?;

    let ws_tx = adapt_WsSender_to_Conduit(ws_tx).await?;
    let ws_rx = adapt_tokio_receiver_to_Conduit(ws_rx);

    // Register the portal with the nexus actor
    let portal_identifier = url.to_string();
    let portal = call_t!(
        nexus,
        NexusActorMessage::Connected,
        100,
        portal_identifier.clone(),
        ws_tx
    )?;

    info!("Portal actor started for: {}", url);

    let portal_actor_copy = portal.clone();
    tokio::spawn(async move {
        conduit::receive_loop(ws_rx, portal_identifier, portal_actor_copy).await
    });

    Ok(portal)
}

pub async fn init(
    url: String,
) -> Result<(ActorRef<NexusActorMessage>, ActorRef<PortalActorMessage>, Receiver<UiUpdate>), anyhow::Error> {
    let nexus = start_nexus(None).await.unwrap();
    let portal = connect_to_server(nexus.clone(), url).await?;

    
    let (ui_update_tx, ui_update_rx)  = std::sync::mpsc::channel();

    let ui_update_tx_copy = ui_update_tx.clone();
    let (local_chat_client, _) = FnActor::<ChatClientMessage>::start_fn(async move |mut ctx| {
        while let Some(msg) = ctx.rx.recv().await {
            match msg {
                ChatClientMessage::MessageReceived(user_alias, chat_msg) => {
                    ui_update_tx_copy.send(UiUpdate::MessageReceived(user_alias, chat_msg))
                        .unwrap();
                }
                ChatClientMessage::UserConnected(user_alias) => {
                    ui_update_tx_copy.send(UiUpdate::UserConnected(user_alias)).unwrap();
                }
                ChatClientMessage::Disconnect => {
                    ui_update_tx_copy.send(UiUpdate::Disconnected).unwrap();
                }
            }
        }
    })
    .await?;

    // the server has published a named actor
    let remote_hub_address = portal
        .ask(
            |rpc| PortalActorMessage::QueryNamedRemoteActor("hub".to_string(), rpc),
            None,
        )
        .await??;

    let remote_hub: ActorRef<shared::HubMessage> = portal
        .instantiate_proxy_for_remote_actor(remote_hub_address)
        .await?;

    let (user_alias, remote_chat_server) = remote_hub
    .ask(
        |rpc| shared::HubMessage::Connect(local_chat_client, rpc),
        None,
    )
    .await?;

    ui_update_tx.send(UiUpdate::Connected(
        user_alias.clone(),
        remote_chat_server.clone(),
))?;

    Ok((nexus, portal, ui_update_rx))
}

#[cfg(not(target_arch = "wasm32"))]
async fn inner_main() -> eframe::Result {
    env_logger::init(); // Log to stderr (if you run with `RUST_LOG=debug`).

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([400.0, 300.0])
            .with_min_inner_size([300.0, 220.0])
            .with_icon(
                // NOTE: Adding an icon is optional
                eframe::icon_data::from_png_bytes(&include_bytes!("../assets/icon-256.png")[..])
                    .expect("Failed to load icon"),
            ), 
        ..Default::default()
    };

    let (nexus, portal, ui_rcv) = init("ws://localhost:8080".to_string()).await.unwrap();

    eframe::run_native(
        "eframe template",
        native_options,
        Box::new(|cc| Ok(Box::new(app::TemplateApp::new(cc, portal, ui_rcv)))),
    )
}

#[cfg(target_arch = "wasm32")]
async fn inner_main() -> eframe::Result {

    use eframe::wasm_bindgen::JsCast as _;

    // Redirect `log` message to `console.log` and friends:
    eframe::WebLogger::init(log::LevelFilter::Debug).ok();

    let web_options = eframe::WebOptions::default();

    wasm_bindgen_futures::spawn_local(async {
        let document = web_sys::window()
            .expect("No window")
            .document()
            .expect("No document");

        let canvas = document
            .get_element_by_id("the_canvas_id")
            .expect("Failed to find the_canvas_id")
            .dyn_into::<web_sys::HtmlCanvasElement>()
            .expect("the_canvas_id was not a HtmlCanvasElement");

        let nexus = init("ws://localhost:8080".to_string()).await.unwrap();

        let start_result = eframe::WebRunner::new()
            .start(
                canvas,
                web_options,
                Box::new(|cc| Ok(Box::new(app::TemplateApp::new(cc)))),
            )
            .await;

        // Remove the loading text and spinner:
        if let Some(loading_text) = document.get_element_by_id("loading_text") {
            match start_result {
                Ok(_) => {
                    loading_text.remove();
                }
                Err(e) => {
                    loading_text.set_inner_html(
                        "<p> The app has crashed. See the developer console for details. </p>",
                    );
                    panic!("Failed to start eframe: {e:?}");
                }
            }
        }
    });
}

// When compiling natively:
#[cfg(not(target_arch = "wasm32"))]
#[tokio::main(flavor = "current_thread")]
async fn main() -> eframe::Result {
    inner_main().await
}

// When compiling to web using trunk:
#[cfg(target_arch = "wasm32")]
fn main() {
    inner_main().await
}

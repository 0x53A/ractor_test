#[cfg(any(feature = "websocket_client", feature = "websocket_server"))]
pub mod websocket;

use futures::{Sink, Stream, StreamExt};
use ractor::{ActorRef, call_t};
use std::pin::Pin;

use log::{error, info};

use crate::{
    nexus::{self, NexusActorMessage},
    portal::PortalActorMessage,
};

// -------------------------------------------------------------------------------------------------------

pub enum ConduitMessage {
    Text(String),
    Binary(Vec<u8>),
    Close(Option<String>),
}

pub type ConduitError = anyhow::Error;

/// the sink, from the point of view of the Conduit; that is, the 'tx' end of a websocket for example.
/// The conduit writes messages into it.
pub type ConduitSink = Pin<Box<dyn Sink<ConduitMessage, Error = ConduitError> + Send>>;
/// the source, from the point of view of the Conduit; that is, the 'rx' end of a websocket for example.
/// the Conduit (asynchronously) reads messages from it.
pub type ConduitSource = Pin<Box<dyn Stream<Item = Result<ConduitMessage, ConduitError>> + Send>>;

// -------------------------------------------------------------------------------------------------------

pub async fn receive_loop(
    mut receiver: ConduitSource,
    identifier: String,
    actor_ref: ActorRef<PortalActorMessage>,
) {
    // Process incoming messages
    while let Some(msg) = receiver.next().await {
        match msg {
            Ok(msg) => match msg {
                ConduitMessage::Text(text) => {
                    if let Err(err) = actor_ref.cast(PortalActorMessage::Text(text.to_string())) {
                        error!("Error sending text message to actor: {}", err);
                        break;
                    }
                }
                ConduitMessage::Binary(data) => {
                    if let Err(err) = actor_ref.cast(PortalActorMessage::Binary(data.to_vec())) {
                        error!("Error sending binary message to actor: {}", err);
                        break;
                    }
                }
                ConduitMessage::Close(close_frame) => {
                    info!(
                        "Portal with {} closed because of reason: {:?}",
                        identifier, close_frame
                    );
                    break;
                }
            },
            Err(e) => {
                error!("Error receiving message from {}: {}", e, identifier);
                break;
            }
        }
    }

    info!("Portal with {} closed", identifier);
    let _ = actor_ref.cast(PortalActorMessage::Close);
}

pub async fn from_sink_source(
    nexus: ActorRef<nexus::NexusActorMessage>,
    portal_identifier: String,
    sink: ConduitSink,
    source: ConduitSource,
) -> Result<ActorRef<PortalActorMessage>, ConduitError> {
    let portal = call_t!(
        nexus,
        NexusActorMessage::Connected,
        100,
        portal_identifier.clone(),
        sink
    );

    match portal {
        Ok(portal_actor) => {
            info!("Portal actor started for: {}", portal_identifier);
            receive_loop(source, portal_identifier, portal_actor.clone()).await;
            Ok(portal_actor)
        }
        Err(e) => {
            error!("Error starting portal actor: {}", e);
            Err(e)?
        }
    }
}

mod channels;

use channels::{Channels, OutgoingMessage};
use futures::{SinkExt, TryStreamExt};
use loro::{LoroDoc, TextDelta};
use serde::{Deserialize, Serialize};
use std::{io::Write, sync::Arc};
use tokio::{
    io::{self, AsyncBufReadExt, BufReader},
    net::{
        tcp::{OwnedReadHalf, OwnedWriteHalf},
        TcpListener, TcpStream,
    },
    sync::mpsc::{Receiver, Sender},
};
use tokio_serde::formats::SymmetricalJson;
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};
use tracing::{debug, error};

// I hate Rust sometimes.
type WriteSocket = tokio_serde::SymmetricallyFramed<
    FramedWrite<OwnedWriteHalf, LengthDelimitedCodec>,
    BackendMessage,
    SymmetricalJson<BackendMessage>,
>;
type ReadSocket = tokio_serde::SymmetricallyFramed<
    FramedRead<OwnedReadHalf, LengthDelimitedCodec>,
    BackendMessage,
    SymmetricalJson<BackendMessage>,
>;

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all(serialize = "snake_case", deserialize = "snake_case"))]
#[serde(tag = "type")]
enum ClientMessage {
    AddPeer { address: String },
    PeerAdded { address: String },
    CreateDocument { initial_content: String },
    Change { change: Change },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all(serialize = "snake_case", deserialize = "snake_case"))]
#[serde(tag = "type")]
enum Change {
    Insert { index: usize, text: String },
    Delete { index: usize, len: usize },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum BackendMessage {
    DocumentSync { data: Vec<u8> },
}

pub struct Client {
    doc: LoroDoc,
    listener: TcpListener,
}

impl Client {
    pub fn new(listener: TcpListener) -> Self {
        Client {
            doc: LoroDoc::new(),
            listener,
        }
    }

    pub async fn begin_event_loop(mut self) {
        let (stdin_task_channel_tx, mut stdin_task_channel_rx) = tokio::sync::mpsc::channel(10);
        let (stdout_task_channel_tx, stdout_task_channel_rx) = tokio::sync::mpsc::channel(10);
        let (incoming_task_from_channel_tx, mut incoming_task_from_channel_rx) =
            tokio::sync::mpsc::channel(10);
        let (incoming_task_to_channel_tx, incoming_task_to_channel_rx) =
            tokio::sync::mpsc::channel(1);
        let (outgoing_task_channel_tx, outgoing_task_channel_rx) = tokio::sync::mpsc::channel(10);
        debug!("Channels created");

        let channels = Channels {
            stdin_tx: stdin_task_channel_tx,
            stdout_tx: stdout_task_channel_tx,
            incoming_to_tx: incoming_task_to_channel_tx,
            outgoing_tx: outgoing_task_channel_tx,
        };

        begin_incoming_task(incoming_task_from_channel_tx, incoming_task_to_channel_rx);
        begin_outgoing_task(outgoing_task_channel_rx);
        begin_stdin_task(channels.stdin_tx.clone());
        begin_stdout_task(stdout_task_channel_rx);
        debug!("Tasks started");

        add_doc_change_subsription(&mut self.doc, channels.stdout_tx.clone());
        debug!("Subscribed to document");

        debug!("Entering main event loop");
        loop {
            tokio::select! {
                Ok(socket) = self.listener.accept() => {
                    accept_new_connection(
                        socket,
                        channels.stdout_tx.clone(),
                        channels.incoming_to_tx.clone(),
                        channels.outgoing_tx.clone(),
                    ).await;
                }

                Some(message) = stdin_task_channel_rx.recv() => {
                    handle_stdin_message(&mut self, channels.clone(), message).await;
                }

                Some(data) = incoming_task_from_channel_rx.recv() => {
                    debug!("Main task importing data");
                    self.doc.import(&data).unwrap();
                }
            }
        }
    }
}

fn begin_incoming_task(tx: Sender<Vec<u8>>, mut rx: Receiver<ReadSocket>) {
    tokio::spawn(async move {
        while let Some(mut socket) = rx.recv().await {
            let tx = tx.clone();

            // TODO store join handles so we can cancel tasks when disconnecting.
            tokio::spawn(async move {
                while let Some(message) = socket.try_next().await.unwrap() {
                    debug!("Received from network: {:?}", message);
                    let BackendMessage::DocumentSync { data } = message;
                    tx.send(data).await.unwrap();
                }
            });
        }
    });
}

fn begin_outgoing_task(mut rx: Receiver<OutgoingMessage>) {
    tokio::spawn(async move {
        let mut sockets = Vec::new();

        loop {
            if let Some(message) = rx.recv().await {
                match message {
                    OutgoingMessage::NewSocket(socket) => {
                        sockets.push(socket);
                    }
                    OutgoingMessage::DocumentData(data) => {
                        let message = BackendMessage::DocumentSync { data };
                        debug!("Sending to network: {:?}", message);

                        for socket in sockets.iter_mut() {
                            socket.send(message.clone()).await.unwrap();
                        }
                    }
                }
            }
        }
    });
}

fn begin_stdin_task(tx: Sender<ClientMessage>) {
    tokio::spawn(async move {
        let stdin = BufReader::new(io::stdin());
        let mut lines = stdin.lines();

        while let Ok(Some(line)) = lines.next_line().await {
            let message = serde_json::from_str::<ClientMessage>(&line).unwrap();
            debug!("Received message from stdin: {:?}", message);
            tx.send(message).await.unwrap();
        }
    });
}

fn begin_stdout_task(mut rx: Receiver<ClientMessage>) {
    tokio::spawn(async move {
        while let Some(message) = rx.recv().await {
            let serialized = serde_json::to_string(&message).unwrap();
            debug!("Sending message to stdout: {:?}", serialized);
            // TODO should this be using Tokio's stdout?
            let mut stdout = std::io::stdout();
            stdout.write_all(serialized.as_bytes()).unwrap();
            stdout.write_all(b"\n").unwrap();
        }
    });
}

fn add_doc_change_subsription(doc: &mut LoroDoc, channel: Sender<ClientMessage>) {
    doc.subscribe_root(Arc::new(move |change| {
        if !change.triggered_by.is_import() {
            return;
        }

        let mut changes = Vec::new();
        for event in change.events {
            let diffs = event.diff.as_text().unwrap();
            let mut index = 0;

            for diff in diffs {
                match diff {
                    TextDelta::Retain { retain, .. } => {
                        index += retain;
                    }
                    TextDelta::Insert { insert, .. } => {
                        changes.push(Change::Insert {
                            index,
                            text: insert.to_string(),
                        });
                    }
                    TextDelta::Delete { delete, .. } => {
                        changes.push(Change::Delete {
                            index,
                            len: *delete,
                        });
                    }
                }
            }
        }

        // We have to spawn a new task here because this callback can't
        // be async, and we can't use `blocking_send` because this runs
        // inside a Tokio thread, which should never block (and will
        // panic if it does).
        let stdout_task_channel_tx = channel.clone();
        tokio::spawn(async move {
            for change in changes {
                let message = ClientMessage::Change { change };
                stdout_task_channel_tx.send(message).await.unwrap();
            }
        });
    }));
}

async fn accept_new_connection(
    (socket, addr): (TcpStream, std::net::SocketAddr),
    stdout_task_channel_tx: Sender<ClientMessage>,
    incoming_task_to_channel_tx: Sender<ReadSocket>,
    outgoing_task_channel_tx: Sender<OutgoingMessage>,
) {
    let (read, write) = socket.into_split();

    let read_framed = tokio_serde::SymmetricallyFramed::new(
        FramedRead::new(read, LengthDelimitedCodec::new()),
        SymmetricalJson::<BackendMessage>::default(),
    );
    let write_framed = tokio_serde::SymmetricallyFramed::new(
        FramedWrite::new(write, LengthDelimitedCodec::new()),
        SymmetricalJson::<BackendMessage>::default(),
    );

    incoming_task_to_channel_tx.send(read_framed).await.unwrap();
    outgoing_task_channel_tx
        .send(OutgoingMessage::NewSocket(write_framed))
        .await
        .unwrap();

    debug!("Accepted connection from peer at {}", addr);
    stdout_task_channel_tx
        .send(ClientMessage::PeerAdded {
            address: addr.to_string(),
        })
        .await
        .unwrap();
}

async fn handle_stdin_message(client: &mut Client, channels: Channels, message: ClientMessage) {
    debug!("Main task received from stdin: {:?}", message);

    match message {
        // Messages that should only ever be sent to the client.
        ClientMessage::PeerAdded { .. } => {
            error!(
                "Received message which should only be sent to the client: {:?}",
                message
            );
        }
        ClientMessage::AddPeer { address } => {
            debug!("Connecting to peer at {}", address);
            let socket = TcpStream::connect(&address).await.unwrap();
            socket.set_nodelay(true).unwrap();

            let (read, write) = socket.into_split();
            let read_framed = tokio_serde::SymmetricallyFramed::new(
                FramedRead::new(read, LengthDelimitedCodec::new()),
                SymmetricalJson::<BackendMessage>::default(),
            );
            let write_framed = tokio_serde::SymmetricallyFramed::new(
                FramedWrite::new(write, LengthDelimitedCodec::new()),
                SymmetricalJson::<BackendMessage>::default(),
            );

            channels.incoming_to_tx.send(read_framed).await.unwrap();
            channels
                .outgoing_tx
                .send(OutgoingMessage::NewSocket(write_framed))
                .await
                .unwrap();

            debug!("Connected to peer at {}", address);
            channels
                .stdout_tx
                .send(ClientMessage::PeerAdded { address })
                .await
                .unwrap();
        }
        ClientMessage::Change { change } => {
            match change {
                Change::Insert { index, text } => {
                    client.doc.get_text("text").insert(index, &text).unwrap();
                }
                Change::Delete { index, len } => {
                    client.doc.get_text("text").delete(index, len).unwrap();
                }
            }

            channels
                .outgoing_tx
                .send(OutgoingMessage::DocumentData(
                    client.doc.export_from(&Default::default()),
                ))
                .await
                .unwrap();
        }
        ClientMessage::CreateDocument { initial_content } => {
            client.doc.get_text("text").update(&initial_content);
            channels
                .outgoing_tx
                .send(OutgoingMessage::DocumentData(
                    client.doc.export_from(&Default::default()),
                ))
                .await
                .unwrap();
        }
    }
}

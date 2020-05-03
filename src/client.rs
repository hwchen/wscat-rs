use ansi_term::Colour::{Green, Red, White};
use anyhow::{Context as _, Result};
use async_tungstenite::tungstenite::Message;
use futures::{future, pin_mut};
use futures::stream::StreamExt;
use linefeed::{ReadResult, Signal};
use smol::{Async, Task};
use std::net::TcpStream;
use std::process;
use std::sync::mpsc::channel;
use std::sync::Arc;
use std::thread;
use url::Url;

// Three threads:
// - stdin loop
// - stdout loop
// - websocket async (read and write tasks spawned)
//
// Use channels to communicate across threads.
// - Crossbeam channel when receiver is in sync stdout
// - piper when receiver is in websocket async
//
// First just support ws, not wss
pub fn wscat_client(url: Url, _auth_option: Option<String>) -> Result<()> {
    // set up channels for communicating
    let (tx_to_stdout, rx_stdout) = channel::<Message>(); // async -> sync
    let (tx_to_ws_write, rx_ws_write) = piper::chan::<Message>(10); // sync -> async, async -> async

    let chans = WsChannels {
        tx_to_ws_write: tx_to_ws_write.clone(),
        tx_to_stdout,
        rx_ws_write,
    };

    // run read/write tasks for websocket
    let ws_handle = thread::spawn(|| smol::run(ws_client(url, chans)));

    // readline interface, which will hold read/write locks
    let readline = linefeed::Interface::new("manx")?;
    readline.set_prompt("> ")?;
    readline.set_report_signal(Signal::Interrupt, true);
    let readline = Arc::new(readline);

    //stdout loop
    let stdout_readline = readline.clone();
    let stdout_handle = thread::spawn(move || {
        for message in rx_stdout {
            if !(message.is_text() || message.is_binary()) {
                continue;
            }
            let mut w = stdout_readline.lock_writer_erase().unwrap();
            writeln!(w, "<< {}", message.into_text().unwrap()).unwrap();
        }
    });

    // stdin loop
    loop {
        match readline.read_line()? {
            ReadResult::Input(input) => {
                readline.add_history(input.clone());
                // block on this
                smol::block_on(tx_to_ws_write.send(Message::text(input)));
            },
            ReadResult::Signal(sig) => {
                // If I don't exit process here, readline loop exits on first Interrupt, and then
                // the rest of the program exists on the second Interrupt
                if sig == Signal::Interrupt { process::exit(0) };
            },
            _ => break,
        }
    }

    ws_handle.join().unwrap().unwrap();
    stdout_handle.join().unwrap();

    Ok(())
}

// only use thread-local executor, since smol will only run on one thread
async fn ws_client(url: Url, chans: WsChannels) -> Result<()> {
    let WsChannels {tx_to_ws_write, tx_to_stdout, rx_ws_write } = chans;
    let tx_to_ws_write = tx_to_ws_write.clone();

    let host = url.host_str().context("can't parse host")?;
    let port = url.port_or_known_default().context("can't guess port")?;
    let addr = format!("{}:{}", host, port);

    let stream = Async::<TcpStream>::connect(&addr).await?;
    let (stream, _resp) = async_tungstenite::client_async(&url, stream).await?;

    let (writer, mut reader) = stream.split();

    // read task reads from ws, then sends signal to stdout loop
    let read_task = Task::local(async move {
        while let Some(message) = reader.next().await {
            let message: Message = match message {
                Ok(m) => m,
                Err(err) => {
                    let out = format!("Connection Closed: {}", err);
                    println!("");
                    println!("{}", Red.paint(out));
                    process::exit(1);
                },
            };

            //write to stdout depending on opcode
            let out = match message {
                Message::Ping(payload) => {
                    tx_to_ws_write.send(Message::Pong(payload)).await;
                    format!("{}", Green.paint("Ping!\n")) //add color
                },
                Message::Text(payload) => { payload },
                Message::Binary(payload) => {
                    // Binary just supported as text here; no downloading, etc.
                    String::from_utf8(payload).unwrap()
                },
                Message::Close(_) => {
                    println!("");
                    let out = format!("{}", Red.paint("Connection Closed: Close message received"));
                    println!("{}", out);
                    process::exit(0);
                },
                _ => format!("Unsupported ws message"),
            };

            // blocking
            // TODO try crossbeam channel?
            tx_to_stdout.send(Message::text(out)).unwrap();
        }
    });

    // TODO remove this unwrap
    let write_task = Task::local(async {
        rx_ws_write.map(Ok).forward(writer).await
    });

    pin_mut!(read_task, write_task);
    future::select(read_task, write_task).await;

    Ok(())
}

struct WsChannels {
    tx_to_ws_write: piper::Sender<Message>,
    tx_to_stdout: std::sync::mpsc::Sender<Message>,
    rx_ws_write: piper::Receiver<Message>,
}


// TODO do this later
// refactor to use from_str
//pub fn parse_authorization(user_password: &str) -> Option<Authorization<Basic>> {
//    let v: Vec<_> = user_password.split(':').collect();
//    if v.len() > 2 {
//        None
//    } else {
//        Some(Authorization (
//            Basic {
//                username: v[0].to_owned(),
//                password: v.get(1).map(|&p| p.to_owned()),
//            }
//        ))
//    }
//}

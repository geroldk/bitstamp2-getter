use chrono::prelude::*;
//use env_logger;
//use log::{debug, error, info};
use log;
use slog::{slog_o, Duplicate};
use slog_term;
use slog_async;
use slog_stdlog;
use slog_scope::{info,error, warn, debug};
use slog::Drain;
use slog_journald;
use slog_envlogger;

use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use ws::{connect, CloseCode, Error, Handler, Handshake, Message, Result, Sender, ErrorKind, Frame, OpCode};
use ws::util::{Timeout, Token};
use std::str::from_utf8;


const URL: &str = "wss://ws.bitstamp.net/";

const PING: Token = Token(1);
const EXPIRE: Token = Token(2);
const PING_VALUE: u64 = 1_000;
const EXPIRE_VALUE: u64 = 2_000;

#[derive(Serialize, Deserialize, Debug)]
struct TraidingPairs {
    url_symbol: String,
}

#[derive(Debug)]
struct Symbol(String);

struct Client<'a> {
    out: Sender,
    symbols: &'a [Symbol],
    fs_ts: Option<String>,
    file: Option<File>,
    ping_timeout: Option<Timeout>,
    expire_timeout: Option<Timeout>,
}

impl Client<'_> {
    fn new(out: Sender, symbols: &[Symbol]) -> Client {
        Client {
            out,
            symbols,
            fs_ts: None,
            file: None,
            ping_timeout: None,
            expire_timeout: None,

        }
    }

    fn build_file_name(ts: &str) -> String {
        let n = format!("data/bitstamp2-ws-{}.log", ts);
        info!("{}", n);
        n
    }
    fn create_file(name: &str) -> File {
        let path = Path::new(name);
        OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
            .unwrap()
    }

    fn write(&mut self, buf: &[u8]) -> std::result::Result<usize, std::io::Error> {
        let utc: DateTime<Utc> = Utc::now();
        let fs_ts = utc.format("%Y-%m-%d_%HZ").to_string();

        match self.fs_ts.as_ref() {
            Some(x) => {
                if x == &fs_ts {
                    //self.file.as_mut().unwrap().write(buf)
                } else {
                    debug!("timestamp: {}", fs_ts);
                    let f = self.file.as_mut().unwrap();

                    f.flush().unwrap();
                    f.sync_all().unwrap();

                    self.fs_ts = Some(fs_ts);
                    self.file = Some(Client::create_file(&Client::build_file_name(
                        &self.fs_ts.as_ref().unwrap(),
                    )));
                    //self.file.as_mut().unwrap().write(buf)
                }
            }
            None => {
                debug!("timestamp: {}", fs_ts);

                self.fs_ts = Some(fs_ts);
                self.file = Some(Client::create_file(&Client::build_file_name(
                    &self.fs_ts.as_ref().unwrap(),
                )));
                //self.file.as_mut().unwrap().write(buf)
            }
        };
        self.file.as_mut().unwrap().write(buf)
    }
}

#[derive(Serialize, Deserialize, Debug)]
struct SubscribeMessageData {
    channel: String,
}

#[derive(Serialize, Deserialize, Debug)]
struct SubscribeMessage {
    event: String,
    data: SubscribeMessageData,
}

const ALL_SUBSCRIPTION_TOPICS: [&str; 5] = [
    "live_trades",
    "live_orders",
    "order_book",
    "detail_order_book",
    "diff_order_book",
];

impl Handler for Client<'_> {
    fn on_open(&mut self, shake: Handshake) -> Result<()> {
        if let Some(addr) = shake.remote_addr()? {
            info!("Connection with {} now open", addr);
        }
        self.out.timeout(PING_VALUE, PING)?;
        self.out.timeout(EXPIRE_VALUE, EXPIRE)?;

        for topic in ALL_SUBSCRIPTION_TOPICS.iter() {
            for symbol in self.symbols.iter() {
                let m = SubscribeMessage {
                    event: "bts:subscribe".to_owned(),
                    data: SubscribeMessageData {
                        channel: format!("{}_{}", topic, symbol.0),
                    },
                };

                let j = serde_json::to_string(&m).unwrap();
                self.out.send(Message::Text(j))?;
            }
        }
        info!("subscribed");
        Ok(())
    }
    fn on_message(&mut self, msg: Message) -> Result<()> {
        //println!("{:?}", msg);
        let utc: DateTime<Utc> = Utc::now();
        if let Message::Text(s) = msg {
            let ts = format!("{:?}", utc);
            let mut mm = String::with_capacity(s.len() + ts.len() + 3);
            mm.push_str(&ts);
            mm.push_str(", ");
            mm.push_str(&s);
            mm.push_str("\n");
            self.write(mm.as_bytes()).unwrap();
        } else {
            error!("{:?}", msg);
        }
        Ok(())
    }
    fn on_close(&mut self, code: CloseCode, reason: &str) {
        info!("Connection closing due to ({:?}) {}", code, reason);
        if let Some(t) = self.ping_timeout.take() {
            self.out.cancel(t).unwrap();
        }
        if let Some(t) = self.expire_timeout.take() {
            self.out.cancel(t).unwrap();
        }
        self.out.shutdown().unwrap();
        panic!("Connection close");
        //self.out.connect(url::Url::parse(URL).unwrap()).unwrap();
    }
    fn on_error(&mut self, err: Error) {

        self.out.shutdown().unwrap();
        error!("{:?}", err);
    }
    fn on_timeout(&mut self, event: Token) -> Result<()> {
        match event {
            // PING timeout has occured, send a ping and reschedule
            PING => {
                self.out.ping(time::precise_time_ns().to_string().into())?;
                self.ping_timeout.take();
                self.out.timeout(PING_VALUE, PING)
            }
            // EXPIRE timeout has occured, this means that the connection is inactive, let's close
            EXPIRE => {error!("TIMEOUT"); self.out.shutdown()},
            // No other timeouts are possible
            _ => {println!("T???"); Err(Error::new(
                ErrorKind::Internal,
                "Invalid timeout token encountered!",
            ))},
        }
    }

    fn on_new_timeout(&mut self, event: Token, timeout: Timeout) -> Result<()> {
        // Cancel the old timeout and replace.
        if event == EXPIRE {
            if let Some(t) = self.expire_timeout.take() {
                self.out.cancel(t)?
            }
            self.expire_timeout = Some(timeout)
        } else {
            // This ensures there is only one ping timeout at a time
            if let Some(t) = self.ping_timeout.take() {
                self.out.cancel(t)?
            }
            self.ping_timeout = Some(timeout)
        }

        Ok(())
    }
    fn on_frame(&mut self, frame: Frame) -> Result<Option<Frame>> {
        // If the frame is a pong, print the round-trip time.
        // The pong should contain data from out ping, but it isn't guaranteed to.
        if frame.opcode() == OpCode::Pong {
            if let Ok(pong) = from_utf8(frame.payload())?.parse::<u64>() {
                let now = time::precise_time_ns();
                //println!("RTT is {:.3}ms.", (now - pong) as f64 / 1_000_000f64);
            } else {
                println!("Received bad pong.");
            }
        }

        // Some activity has occured, so reset the expiration
        self.out.timeout(EXPIRE_VALUE, EXPIRE)?;

        // Run default frame validation
        DefaultHandler.on_frame(frame)
    }
}

struct DefaultHandler;

impl Handler for DefaultHandler {}

fn setup_logging() -> slog_scope::GlobalLoggerGuard {
    //let decorator = slog_term::TermDecorator::new().build();
    //let drain_term = slog_async::Async::new(slog_term::FullFormat::new(decorator).build().fuse()).build().fuse();
    let drain = slog_async::Async::new(slog_journald::JournaldDrain.fuse()).build().fuse();
    //let drain = Duplicate::new(drain_term, drain).fuse();
   // let drain = slog_envlogger::new( drain).fuse();
    let drain = slog_async::Async::new(drain).build().fuse();
    let logger = slog::Logger::root(drain, slog_o!("version" => env!("CARGO_PKG_VERSION")));
    let _scope_guard = slog_scope::set_global_logger(logger);
    let _log_guard = slog_stdlog::init_with_level(log::Level::Info).unwrap();
    _scope_guard
}

fn main() {
    let _logging_guard = setup_logging();
    let resp = ureq::get("https://www.bitstamp.net/api/v2/trading-pairs-info/").call();

    // .ok() tells if response is 200-299.
    if resp.ok() {
        let j: Vec<TraidingPairs> = serde_json::from_reader(resp.into_reader()).unwrap();
        //println!("{:?}",j);
        let jj: Vec<Symbol> = j.into_iter().map(|x| Symbol(x.url_symbol)).collect();
        info!("{}", "INFO"; "APP" => "BITSTAMP2");
        debug!("{:?}", jj);
        connect(URL, |out| Client::new(out, &jj)).unwrap();
    }
    std::process::exit(1);

}

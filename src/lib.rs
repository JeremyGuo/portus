#![feature(box_patterns)]
#![cfg_attr(feature = "bench", feature(test))]

extern crate bytes;
extern crate clap;
extern crate libc;
extern crate nix;
#[macro_use]
extern crate nom;

#[macro_use]
extern crate slog;
extern crate slog_async;
extern crate slog_term;

pub mod ipc;
pub mod lang;
#[macro_use]
pub mod pattern;
pub mod serialize;
#[macro_use]
pub mod algs;

use std::collections::HashMap;

use ipc::Ipc;
use ipc::Backend;
use serialize::Msg;

#[derive(Debug)]
pub struct Error(pub String);

impl<T: std::error::Error + std::fmt::Display> From<T> for Error {
    fn from(e: T) -> Error {
        Error(format!("err {}", e))
    }
}

pub type Result<T> = std::result::Result<T, Error>;

use std::rc::Rc;
pub struct Datapath<T: Ipc>(Rc<Backend<T>>);

use lang::{Reg, Scope};
impl<T: Ipc> Datapath<T> {
    /// Algorithm implementations use send_pattern() to control the datapath's behavior by
    /// calling send_pattern() with:
    /// 1. An initialized backend b.
    /// 2. The flow's sock_id. IPC implementations supporting addressing (e.g. Unix sockets, which can
    /// communicate with many applications using UDP datapaths)  MUST make the address be sock_id
    /// 3. The control pattern prog to install. Implementations can create patterns using make_pattern!.
    /// send_pattern() will return quickly with a Result indicating whether the send was successful.
    pub fn send_pattern(&self, sock_id: u32, prog: pattern::Pattern) -> Result<()> {
        let msg = serialize::pattern::Msg {
            sid: sock_id,
            num_events: prog.len() as u32,
            pattern: prog,
        };

        let buf = serialize::serialize(&msg)?;
        self.0.send_msg(&buf[..])?;
        Ok(())
    }

    pub fn install_measurement(&self, sock_id: u32, src: &[u8]) -> Result<Scope> {
        let (bin, sc) = lang::compile(src)?;
        let msg = serialize::install_fold::Msg {
            sid: sock_id,
            num_instrs: bin.0.len() as u32,
            instrs: bin,
        };

        let buf = serialize::serialize(&msg)?;
        self.0.send_msg(&buf[..])?;
        Ok(sc)
    }
}

pub struct Measurement {
    fields: Vec<u64>,
}

impl Measurement {
    pub fn get_field(&self, field: &str, sc: &Scope) -> Option<u64> {
        sc.get(field).and_then(|r| match *r {
            Reg::Perm(idx, _) => {
                if idx as usize >= self.fields.len() {
                    return None;
                }

                Some(self.fields[idx as usize])
            },
            _ => None,
        })
    }
}

pub trait CongAlg<T: Ipc> {
    type Config: Clone;
    fn name() -> String;
    fn create(control: Datapath<T>, cfg: Config<T, Self>, info: DatapathInfo) -> Self;
    fn measurement(&mut self, sock_id: u32, m: Measurement);
    fn close(&mut self) {} // default implementation does nothing (optional method)
}

pub struct Config<I, U: ?Sized>
where
    I: Ipc,
    U: CongAlg<I>,
{
    pub logger: Option<slog::Logger>,
    pub config: U::Config,
}

// Cannot #[derive(Clone)] on Config because the compiler does not realize
// we are not using I or U, only U::Config.
// https://github.com/rust-lang/rust/issues/26925
impl<I, U> Clone for Config<I, U>
where
    I: Ipc,
    U: CongAlg<I>,
{
    fn clone(&self) -> Self {
        Config {
            logger: self.logger.clone(),
            config: self.config.clone(),
        }
    }
}

#[derive(Clone)]
pub struct DatapathInfo {
    pub sock_id: u32,
    pub init_cwnd: u32,
    pub mss: u32,
    pub src_ip: u32,
    pub src_port: u32,
    pub dst_ip: u32,
    pub dst_port: u32,
}

/// Main execution loop of ccp for the static pipeline use case.
/// Blocks "forever".
/// In this use case, an algorithm implementation is a binary which
/// 1. Initializes an ipc backend (depending on datapath)
/// 2. Calls `start()`, passing the `Backend b` and a `Config` with optional
/// logger and command line argument structure.
///
/// `start()`:
/// 1. listens for messages from the datapath
/// 2. call the appropriate message in `U: impl CongAlg`
///
/// `start()` will never return (`-> !`). It will panic if:
/// 1. It receives a `pattern` or `install_fold` control message (only a datapath should receive these)
/// 2. The IPC channel fails.
pub fn start<I, U>(b: Backend<I>, cfg: &Config<I, U>, blocking: ipc::ListenMode) -> !
where
    I: Ipc,
    U: CongAlg<I>,
{
    let mut flows = HashMap::<u32, U>::new();
    let backend = std::rc::Rc::new(b);
    for m in backend.listen(blocking).iter() {
        if let Ok(msg) = Msg::from_buf(&m[..]) {
            match msg {
                Msg::Cr(c) => {
                    if flows.remove(&c.sid).is_some() {
                        cfg.logger.as_ref().map(|log| {
                            debug!(log, "re-creating already created flow"; "sid" => c.sid);
                        });
                    }

                    cfg.logger.as_ref().map(|log| {
                        debug!(log, "creating new flow"; 
                               "sid" => c.sid, 
                               "init_cwnd" => c.init_cwnd,
                               "mss"  =>  c.mss,
                               "src_ip"  =>  c.src_ip,
                               "src_port"  =>  c.src_port,
                               "dst_ip"  =>  c.dst_ip,
                               "dst_port"  =>  c.dst_port,
                        );
                    });

                    let alg = U::create(
                        Datapath(backend.clone()),
                        cfg.clone(),
                        DatapathInfo {
                            sock_id: c.sid,
                            init_cwnd: c.init_cwnd,
                            mss: c.mss,
                            src_ip: c.src_ip,
                            src_port: c.src_port,
                            dst_ip: c.dst_ip,
                            dst_port: c.dst_port,
                        },
                    );
                    flows.insert(c.sid, alg);
                }
                Msg::Ms(m) => {
                    if flows.contains_key(&m.sid) {
                        if m.num_fields == 0 {
                            let mut alg = flows.remove(&m.sid).unwrap();
                            alg.close();
                        } else {
                            let alg = flows.get_mut(&m.sid).unwrap();
                            alg.measurement(m.sid, Measurement { fields: m.fields })
                        }
                    } else {
                        cfg.logger.as_ref().map(|log| {
                            debug!(log, "measurement for unknown flow"; "sid" => m.sid);
                        });
                    }
                }
                Msg::Pt(_) | Msg::Fld(_) => {
                    panic!(
                        "The start() listener should never receive a pattern \
                        or install_fold message, since it is on the CCP side."
                    )
                }
                _ => continue,
            }
        }
    }

    panic!("The IPC receive channel closed.");
}

#[cfg(test)]
mod test;

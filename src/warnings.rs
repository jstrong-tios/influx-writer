//! An object to handle everyone's errors
//! 

use std::thread::{self, JoinHandle};
use std::sync::{Arc, Mutex, RwLock};
use std::sync::mpsc::{self, Sender, Receiver, channel};
use std::collections::{BTreeMap, VecDeque};
use std::fmt::{self, Display, Error as FmtError, Formatter};
use std::io::{self, Read, Write};
use std::fs;

use zmq;
use chrono::{DateTime, Utc, TimeZone};
use termion::color::{self, Fg, Bg};
use influent::measurement::{Measurement, Value as InfluentValue};
use slog::{self, OwnedKVList, Drain, Key, KV, Level, Logger};
use sloggers::types::Severity;

use super::{nanos, file_logger};
use influx;


const N_WARNINGS: usize = 500;

#[macro_export]
macro_rules! confirmed {
    ($warnings:ident, $($args:tt)*) => (
        {
            let _ = warnings.send(Warning::Confirmed( ( format!($($args)*) ) ) ).unwrap();
        }
    )
}

/// logs a `Warning::Awesome` message to the `WarningsManager`
#[macro_export]
macro_rules! awesome {
    ($warnings:ident, $($args:tt)*) => (
        {
            let _ = $warnings.send(Warning::Awesome( ( format!($($args)*) ) ) ).unwrap();
        }
    )
}

#[macro_export]
macro_rules! critical {
    ($warnings:ident, $($args:tt)*) => (
        {
            let _ = $warnings.send(Warning::Critical( ( format!($($args)*) ) ) ).unwrap();
        }
    )
}

#[macro_export]
macro_rules! notice {
    ($warnings:ident, $($args:tt)*) => (
        {
            let _ = $warnings.send(Warning::Notice( ( format!($($args)*) ) ) ).unwrap();
        }
    )
}

#[macro_export]
macro_rules! error {
    ($warnings:ident, $($args:tt)*) => (
        {
            $warnings.send(Warning::Error( ( format!($($args)*) ) ) ).unwrap();
        }
    )
}

/// represents a non-fatal error somewhere in
/// the system to report either to the program interface
/// or in logs.
/// 
#[derive(Debug, Clone, PartialEq)]
pub enum Warning {
    Notice(String),

    Error(String),

    DegradedService(String),

    Critical(String),

    Confirmed(String),

    Awesome(String),

    Log {
        level: Level,
        module: &'static str,
        function: &'static str,
        line: u32,
        msg: String,
        kv: MeasurementRecord,
    },

    Terminate
}

impl Warning {
    pub fn msg(&self) -> String {
        match *self {
            Warning::Notice(ref s) | Warning::Error(ref s) | 
            Warning::DegradedService(ref s) | Warning::Critical(ref s) | 
            Warning::Awesome(ref s) | Warning::Confirmed(ref s) |
            Warning::Log { msg: ref s, .. } => 
                s.clone(),

            Warning::Terminate => "".to_owned()
        }
    }
    pub fn msg_str(&self) -> &str {
        match *self {
            Warning::Notice(ref s) | Warning::Error(ref s) |
            Warning::DegradedService(ref s) | Warning::Critical(ref s) |
            Warning::Awesome(ref s) | Warning::Confirmed(ref s) |
            Warning::Log { msg: ref s, .. } => 

                s.as_ref(),

            Warning::Terminate => "Terminate"
        }
    }

    pub fn category_str(&self) -> &str {
        match self {
            &Warning::Notice(_) => "NOTC",
            &Warning::Error(_) => "ERRO",
            &Warning::Critical(_) => "CRIT",
            &Warning::DegradedService(_) => "DGRD",
            &Warning::Confirmed(_) => "CNFD",
            &Warning::Awesome(_) => "AWSM",
            &Warning::Log { ref level, .. } => level.as_short_str(),
            &Warning::Terminate => "TERM",
        }
    }

    pub fn category(&self, f: &mut Formatter) -> fmt::Result {
        match *self {
            Warning::Notice(_) => {
                write!(f, "[ Notice ]")
            }

            Warning::Error(_) => {
                write!(f, "{yellow}[{title}]{reset}", 
                    yellow = Fg(color::LightYellow),
                    title = " Error--",
                    reset = Fg(color::Reset))
            }

            Warning::Critical(_) => {
                write!(f, "{bg}{fg}{title}{resetbg}{resetfg}", 
                        bg = Bg(color::Red),
                        fg = Fg(color::White),
                        title = " CRITICAL ",
                        resetbg = Bg(color::Reset),
                        resetfg = Fg(color::Reset))
            }

            Warning::Awesome(_) => {
                write!(f, "{color}[{title}]{reset}", 
                        color = Fg(color::Green),
                        title = "Awesome!",
                        reset = Fg(color::Reset))
            }

            Warning::DegradedService(_) => {
                write!(f, "{color}[{title}] {reset}", 
                        color = Fg(color::Blue),
                        title = "Degraded Service ",
                        reset = Fg(color::Reset))
            }

            Warning::Confirmed(_) => {
                write!(f, "{bg}{fg}{title}{resetbg}{resetfg}", 
                        bg = Bg(color::Blue),
                        fg = Fg(color::White),
                        title = "Confirmed ",
                        resetbg = Bg(color::Reset),
                        resetfg = Fg(color::Reset))
            }

            _ => Ok(())
        }
    }
}

impl Display for Warning {
    fn fmt(&self, f: &mut Formatter) -> Result<(), FmtError> {
        self.category(f);
        write!(f, " {}", self.msg())
    }
}

// impl Message for Warning {
//     fn kill_switch() -> Self {
//         Warning::Terminate
//     }
// }

#[derive(Debug, Clone)]
pub struct Record {
    pub time: DateTime<Utc>,
    pub msg: Warning
}

impl Record {
    pub fn new(msg: Warning) -> Self {
        let time = Utc::now();
        Record { time, msg }
    }

    pub fn to_measurement(&self, name: &'static str) -> Measurement {
        let cat = self.msg.category_str();
        let body = self.msg.msg_str();
        let mut m = Measurement::new(name);
        m.add_tag("category", cat);
        m.add_field("msg", InfluentValue::String(body));
        m.set_timestamp(nanos(self.time) as i64);
        m
    }
}

impl Display for Record {
    fn fmt(&self, f: &mut Formatter) -> Result<(), FmtError> {
        write!(f, "{} | {}", self.time.format("%H:%M:%S"), self.msg)
    }
}

pub type SlogResult = Result<(), slog::Error>;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    String(String),
    Float(f64),
    Integer(i64),
    Boolean(bool)
}

impl Value {
    pub fn to_influent<'a>(&'a self) -> InfluentValue<'a> {
        match self {
            &Value::String(ref s) => InfluentValue::String(s),
            &Value::Float(n) => InfluentValue::Float(n),
            &Value::Integer(i) => InfluentValue::Integer(i),
            &Value::Boolean(b) => InfluentValue::Boolean(b),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MeasurementRecord {
    fields: Vec<(Key, Value)>,
    //measurement: &'a mut Measurement<'a>,
    tags: Vec<(Key, String)>,
}

impl MeasurementRecord {
    pub fn new() -> Self {
        MeasurementRecord {
            fields: Vec::new(),
            tags: Vec::new(),
        }
    }

    pub fn add_field(&mut self, key: Key, val: Value) -> SlogResult {
        self.fields.push((key, val));
        Ok(())
    }

    pub fn add_tag(&mut self, key: Key, val: String) -> SlogResult {
        match key {
            "exchange" | "thread" | "ticker" | "category" => {
                self.tags.push((key, val));
            }

            other => {
                self.add_field(key, Value::String(val));
            }
        }

        Ok(())
    }

    pub fn serialize_values(&mut self, record: &slog::Record, values: &OwnedKVList) {
        let mut builder = TagBuilder { mrec: self };
        values.serialize(record, &mut builder);
    }

    pub fn to_measurement<'a>(&'a self, name: &'a str) -> Measurement<'a> {
        let fields: BTreeMap<&'a str, InfluentValue<'a>> =
            self.fields.iter()
                .map(|&(k, ref v)| {
                    (k, v.to_influent())
                }).collect();

        let tags: BTreeMap<&'a str, &'a str> = 
            self.tags.iter()
                .map(|&(k, ref v)| {
                    (k, v.as_ref())
                }).collect();

        Measurement {
            key: name,
            timestamp: Some(nanos(Utc::now()) as i64),
            fields,
            tags,
        }
    }
}

impl slog::Serializer for MeasurementRecord {
    fn emit_usize(&mut self, key: Key, val: usize) -> SlogResult { self.add_field(key, Value::Integer(val as i64)) }
    fn emit_isize(&mut self, key: Key, val: isize) -> SlogResult { self.add_field(key, Value::Integer(val as i64)) }
    fn emit_bool(&mut self, key: Key, val: bool)   -> SlogResult { self.add_field(key, Value::Boolean(val)); Ok(()) }
    fn emit_u8(&mut self, key: Key, val: u8)       -> SlogResult { self.add_field(key, Value::Integer(val as i64)) }
    fn emit_i8(&mut self, key: Key, val: i8)       -> SlogResult { self.add_field(key, Value::Integer(val as i64)) }
    fn emit_u16(&mut self, key: Key, val: u16)     -> SlogResult { self.add_field(key, Value::Integer(val as i64)) } 
    fn emit_i16(&mut self, key: Key, val: i16)     -> SlogResult { self.add_field(key, Value::Integer(val as i64)) }
    fn emit_u32(&mut self, key: Key, val: u32)     -> SlogResult { self.add_field(key, Value::Integer(val as i64)) }
    fn emit_i32(&mut self, key: Key, val: i32)     -> SlogResult { self.add_field(key, Value::Integer(val as i64)) }
    fn emit_f32(&mut self, key: Key, val: f32)     -> SlogResult { self.add_field(key, Value::Float(val as f64)) }
    fn emit_u64(&mut self, key: Key, val: u64)     -> SlogResult { self.add_field(key, Value::Integer(val as i64)) }
    fn emit_i64(&mut self, key: Key, val: i64)     -> SlogResult { self.add_field(key, Value::Integer(val)) }
    fn emit_f64(&mut self, key: Key, val: f64)     -> SlogResult { self.add_field(key, Value::Float(val)) }
    fn emit_str(&mut self, key: Key, val: &str)    -> SlogResult { self.add_field(key, Value::String(val.to_string())) }
    fn emit_unit(&mut self, key: Key)              -> SlogResult { self.add_field(key, Value::Boolean(true)) }
    fn emit_none(&mut self, key: Key)              -> SlogResult { Ok(()) } //self.add_field(key, Value::String("none".into())) }
    fn emit_arguments(&mut self, key: Key, val: &fmt::Arguments) -> SlogResult { self.add_field(key, Value::String(val.to_string())) } 
}

pub struct TagBuilder<'a> {
    mrec: &'a mut MeasurementRecord
}

impl<'a> slog::Serializer for TagBuilder<'a> {
    fn emit_str(&mut self, key: Key, val: &str)    -> SlogResult { 
        match key {
            "exchange" | "thread" | "ticker" | "category" => {
                self.mrec.add_tag(key, val.to_string())
            }

            other => {
                self.mrec.add_field(key, Value::String(val.to_string()))
            }
        }
    }

    fn emit_arguments(&mut self, key: Key, val: &fmt::Arguments) -> SlogResult { 
        match key {
            "exchange" | "thread" | "ticker" | "category" => {
                self.mrec.add_tag(key, val.to_string())
            }

            other => {
                self.mrec.add_field(key, Value::String(val.to_string()))
            }
        }

    }
}

pub struct WarningsDrain<D: Drain> {
    level: Level,
    tx: Arc<Mutex<Sender<Warning>>>,
    drain: D,
    to_file: Logger,
}

impl<D> WarningsDrain<D> 
    where D: Drain
{
    pub fn new(tx: Sender<Warning>, level: Level, drain: D) -> Self {
        let tx = Arc::new(Mutex::new(tx));
        let to_file = file_logger("var/log/mm.log", Severity::Warning);
        WarningsDrain { tx, drain, level, to_file }
    }
}

impl From<Sender<Warning>> for WarningsDrain<slog::Fuse<slog::Discard>> {
    fn from(tx: Sender<Warning>) -> Self {
        WarningsDrain::new(tx, Level::Debug, slog::Discard.fuse())
    }
}

impl<D: Drain> Drain for WarningsDrain<D> {
    type Ok = ();
    type Err = D::Err;

    fn log(&self, record: &slog::Record, values: &OwnedKVList) -> Result<Self::Ok, Self::Err> {
        if record.level() <= self.level {
            let mut ser = MeasurementRecord::new();
            ser.serialize_values(record, values);
            record.kv().serialize(record, &mut ser);
            let msg = record.msg().to_string();
            if let Ok(lock) = self.tx.lock() {
                let _ = lock.send(Warning::Log { 
                    level: record.level(),
                    module: record.module(),
                    function: record.function(),
                    line: record.line(),
                    msg, 
                    kv: ser 
                });
            }
        }
        if record.level() <= Level::Warning {
            let _ = self.to_file.log(record);
        }
        let _ = self.drain.log(record, values)?;
        Ok(())
    }
}


#[derive(Debug)]
pub struct WarningsManager {
    pub tx: Sender<Warning>,
    pub warnings: Arc<RwLock<VecDeque<Record>>>,
    thread: Option<JoinHandle<()>>
}

impl WarningsManager {
    /// `measurement_name` is the name of the influxdb measurement
    /// we will save log entries to.
    ///
    pub fn new(measurement_name: &'static str) -> Self {
        let warnings = Arc::new(RwLock::new(VecDeque::new()));
        let warnings_copy = warnings.clone();
        let (tx, rx) = channel();
        let mut buf = String::with_capacity(4096);
        let ctx = zmq::Context::new();
        let socket = influx::push(&ctx).unwrap();
        let thread = thread::spawn(move || { 
            let path = format!("var/log/warnings-manager-{}.log", measurement_name);
            let logger = file_logger(&path, Severity::Info);
            info!(logger, "entering loop");
            loop {
                if let Ok(msg) = rx.recv() {
                    match msg {
                        Warning::Terminate => {
                            crit!(logger, "terminating");
                            break;
                        }

                        Warning::Log { level, module, function, line, msg, kv } => {
                            debug!(logger, "new Warning::Debug arrived";
                                   "msg" => &msg);
                            let mut meas = kv.to_measurement(measurement_name);
                            meas.add_field("msg", InfluentValue::String(msg.as_ref()));
                            meas.add_tag("category", level.as_short_str());
                            influx::serialize(&meas, &mut buf);
                            let _ = socket.send_str(&buf, 0);
                            buf.clear();
                            // and don't push to warnings
                            // bc it's debug
                        }

                        other => {
                            debug!(logger, "new {} arrived", other.category_str();
                                   "msg" => other.category_str());
                            let rec = Record::new(other);
                            {
                                let m = rec.to_measurement(measurement_name);
                                influx::serialize(&m, &mut buf);
                                let _ = socket.send_str(&buf, 0);
                                buf.clear();
                            }
                            if let Ok(mut lock) = warnings.write() {
                                lock.push_front(rec);
                                lock.truncate(N_WARNINGS);
                            }
                        }
                    }
                }
            } 
        });

        WarningsManager {
            warnings: warnings_copy,
            thread: Some(thread),
            tx
        }
    }
}

impl Drop for WarningsManager {
    fn drop(&mut self) {
        let _ = self.tx.send(Warning::Terminate);
        if let Some(thread) = self.thread.take() {
            thread.join();
        }
    }
}

pub struct ZmqDrain<D>
    where D: Drain,
{
    drain: D,
    ctx: zmq::Context,
    socket: zmq::Socket,
    buf: Arc<Mutex<Vec<u8>>>
}

impl<D> ZmqDrain<D> 
    where D: Drain,
{
    pub fn new(drain: D) -> Self {
        let _ = fs::create_dir("/tmp/mm");
        let ctx = zmq::Context::new();
        let socket = ctx.socket(zmq::PUB).unwrap();
        socket.bind("ipc:///tmp/mm/log").expect("zmq publisher bind failed");
        let buf = Arc::new(Mutex::new(Vec::with_capacity(4096)));

        ZmqDrain {
            drain,
            ctx,
            socket,
            buf
        }
    }
}

const TIMESTAMP_FORMAT: &'static str = "%b %d %H:%M:%S%.3f";

impl<D> Drain for ZmqDrain<D> 
    where D: Drain
{
    type Ok = D::Ok;
    type Err = D::Err;

    fn log(&self, record: &slog::Record, values: &OwnedKVList) -> Result<Self::Ok, Self::Err> {
        {
            let mut buf = self.buf.lock().unwrap();
            write!(buf, "{time} {level}", 
                time = Utc::now().format(TIMESTAMP_FORMAT),
                level = record.level().as_short_str());
            {
                let mut thread_ser = ThreadSer(&mut buf);
                record.kv().serialize(record, &mut thread_ser);
                values.serialize(record, &mut thread_ser);
            }

            write!(buf, " {file:<20} {line:<5} {msg}",
                   file = record.file(),
                   line = record.line(),
                   msg = record.msg());
                
            {
                let mut kv_ser = KvSer(&mut buf);
                record.kv().serialize(record, &mut kv_ser);
                values.serialize(record, &mut kv_ser);
            }

            let _ = self.socket.send(&buf, 0);
            buf.clear();
        }
        self.drain.log(record, values)
    }
}

/// Can be used as a `Write` with `slog_term` and
/// other libraries. 
///
pub struct ZmqIo {
    ctx: zmq::Context,
    socket: zmq::Socket,
    buf: Vec<u8>
}

impl ZmqIo {
    pub fn new(addr: &str) -> Self {
        let _ = fs::create_dir("/tmp/mm");
        let ctx = zmq::Context::new();
        let socket = ctx.socket(zmq::PUB).unwrap();
        let addr = format!("ipc:///tmp/mm/{}", addr);
        socket.bind(&addr).expect("zmq publisher bind failed");
        let buf = Vec::with_capacity(4096);
        ZmqIo { ctx, socket, buf }
    }
}

impl Write for ZmqIo {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buf.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.buf.pop() {
            Some(b'\n') => {
                let _ = self.socket.send(&self.buf, 0);
            }

            Some(other) => {
                self.buf.push(other);
                let _ = self.socket.send(&self.buf, 0);
            }

            None => {
                return Ok(());
            }
        }
        self.buf.clear();
        Ok(())
    }
}

/// Serializes *only* KV pair with `key == "thread"`
///
struct ThreadSer<'a>(&'a mut Vec<u8>);

impl<'a> slog::ser::Serializer for ThreadSer<'a> {
    fn emit_arguments(&mut self, key: &str, val: &fmt::Arguments) -> slog::Result {
        Ok(())
    }

    fn emit_str(&mut self, key: &str, val: &str) -> slog::Result {
        if key == "thread" {
            write!(self.0, " {:<20}", val);
        }
        Ok(())
    }
}


/// Serializes KV pairs as ", k: v"
///
struct KvSer<'a>(&'a mut Vec<u8>);

macro_rules! s(
    ($s:expr, $k:expr, $v:expr) => {
        try!(write!($s.0, ", {}: {}", $k, $v));
    };
);

impl<'a> slog::ser::Serializer for KvSer<'a> {
    fn emit_none(&mut self, key: &str) -> slog::Result {
        s!(self, key, "None");
        Ok(())
    }
    fn emit_unit(&mut self, key: &str) -> slog::Result {
        s!(self, key, "()");
        Ok(())
    }

    fn emit_bool(&mut self, key: &str, val: bool) -> slog::Result {
        s!(self, key, val);
        Ok(())
    }

    fn emit_char(&mut self, key: &str, val: char) -> slog::Result {
        s!(self, key, val);
        Ok(())
    }

    fn emit_usize(&mut self, key: &str, val: usize) -> slog::Result {
        s!(self, key, val);
        Ok(())
    }
    fn emit_isize(&mut self, key: &str, val: isize) -> slog::Result {
        s!(self, key, val);
        Ok(())
    }

    fn emit_u8(&mut self, key: &str, val: u8) -> slog::Result {
        s!(self, key, val);
        Ok(())
    }
    fn emit_i8(&mut self, key: &str, val: i8) -> slog::Result {
        s!(self, key, val);
        Ok(())
    }
    fn emit_u16(&mut self, key: &str, val: u16) -> slog::Result {
        s!(self, key, val);
        Ok(())
    }
    fn emit_i16(&mut self, key: &str, val: i16) -> slog::Result {
        s!(self, key, val);
        Ok(())
    }
    fn emit_u32(&mut self, key: &str, val: u32) -> slog::Result {
        s!(self, key, val);
        Ok(())
    }
    fn emit_i32(&mut self, key: &str, val: i32) -> slog::Result {
        s!(self, key, val);
        Ok(())
    }
    fn emit_f32(&mut self, key: &str, val: f32) -> slog::Result {
        s!(self, key, val);
        Ok(())
    }
    fn emit_u64(&mut self, key: &str, val: u64) -> slog::Result {
        s!(self, key, val);
        Ok(())
    }
    fn emit_i64(&mut self, key: &str, val: i64) -> slog::Result {
        s!(self, key, val);
        Ok(())
    }
    fn emit_f64(&mut self, key: &str, val: f64) -> slog::Result {
        s!(self, key, val);
        Ok(())
    }
    fn emit_str(&mut self, key: &str, val: &str) -> slog::Result {
        s!(self, key, val);
        Ok(())
    }
    fn emit_arguments(
        &mut self,
        key: &str,
        val: &fmt::Arguments,
    ) -> slog::Result {
        s!(self, key, val);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test::{black_box, Bencher};

    #[test]
    #[ignore]
    fn it_creates_a_logger() {
        let wm = WarningsManager::new("rust-test");
        let im = influx::writer(wm.tx.clone());
        let drain = 
            WarningsDrain { 
                tx: Arc::new(Mutex::new(wm.tx.clone())), 
                drain: slog::Discard,
                to_file: Logger::root(slog::Discard, o!()),
                level: Level::Trace,
            };
        let logger = slog::Logger::root(drain, o!());
        //for _ in 0..60 {
        //    debug!(logger, "test 123"; "exchange" => "plnx");
        //}
    }

    #[bench]
    fn it_sends_integers_with_a_sender_behind_a_mutex(b: &mut Bencher) {
        let (tx, rx) = channel();
        enum Msg {
            Val(usize),
            Terminate
        }
        let worker = thread::spawn(move || {
            let mut xs = Vec::new();
            loop {
                match rx.recv().unwrap() {
                    Msg::Val(x) => { xs.push(x); }
                    Msg::Terminate => break,
                }
            }
            xs.len()
        });
        let tx = Arc::new(Mutex::new(tx));
        b.iter(|| {
            let lock = tx.lock().unwrap();
            let _ = lock.send(Msg::Val(1));
        });
        let _ = tx.lock().unwrap().send(Msg::Terminate);
        let len = worker.join().unwrap();
        //println!("{}", len);

    }
}

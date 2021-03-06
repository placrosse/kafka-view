use chrono::Local;
use env_logger::LogBuilder;
use iron::headers::ContentType;
use iron::{Response, status};
use iron_compress::GzipWriter;
use log::{LogRecord, LogLevelFilter};
use maud::Markup;
use serde_json;

use std::thread;

pub fn setup_logger(log_thread: bool, rust_log: Option<&str>, date_format: &str) {
    let date_format = date_format.to_owned();
    let output_format = move |record: &LogRecord| {
        let thread_name = if log_thread {
            format!("({}) ", thread::current().name().unwrap_or("unknown"))
        } else {
            "".to_string()
        };
        let date = Local::now().format(&date_format).to_string();
        format!("{}: {}{} - {} - {}", date, thread_name, record.level(), record.target(), record.args())
    };

    let mut builder = LogBuilder::new();
    builder.format(output_format).filter(None, LogLevelFilter::Info);

    rust_log.map(|conf| builder.parse(conf));

    builder.init().unwrap();
}

macro_rules! format_error_chain {
    ($err: expr) => {{
        error ! ("error: {}", $err);
        for e in $err.iter().skip(1) {
            error ! ("caused by: {}", e);
        }
        if let Some(backtrace) = $err.backtrace() {
            error ! ("backtrace: {:?}", backtrace);
        }
    }}
}

pub fn gzip_ok_response(markup: Markup) -> Response {
    let mut resp = Response::with((status::Ok, GzipWriter(markup.into_string().as_bytes())));
    resp.headers.set(ContentType::html());
    resp
}

pub fn json_response(json: serde_json::Value) -> Response {
    let mut resp = Response::with((status::Ok, json.to_string()));
    resp.headers.set(ContentType::json());
    resp
}

pub fn json_gzip_response(json: serde_json::Value) -> Response {
    let json_string = json.to_string();
    let gzip_writer = GzipWriter(json_string.as_bytes());
    let mut resp = Response::with((status::Ok, gzip_writer));
    resp.headers.set(ContentType::json());
    resp
}

macro_rules! time {
    ($title:expr, $msg:expr) => {{
        use chrono;
        let start_time = chrono::UTC::now();
        let ret = $msg;
        let elapsed_micros = chrono::UTC::now().signed_duration_since(start_time).num_microseconds().unwrap() as f32;
        println!("Elapsed time while {}: {:.3}ms", $title, elapsed_micros / 1000f32);
        ret
    }};
}

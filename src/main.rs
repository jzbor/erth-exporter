use std::collections::HashMap;
use std::io::BufReader;
use std::io::prelude::*;
use std::io;
use std::net::{TcpListener, TcpStream};
use std::sync::Mutex;
use std::time;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;


/// URL to be scraped
const URL: &str = "https://erlangen.de/themenseite/service/buerger/aktuelle-wartezeit";
/// CSS selector for the queue blocks
const BLOCK_SELECTOR: &str = ".fr-view";
/// CSS selector for the data values
const VALUE_SELECTOR: &str = ".flex>span";
/// Filter for queue blocks
const BLOCK_CONTENT_FILTER: &str = "Wartende Personen";
/// Supported HTTP version
const HTTP_VERSION: &str = "HTTP/1.1";
/// Time-to-live for [cached](CACHED_FRAME) data frames
const CACHE_EXPIRATION: Duration = Duration::from_secs(30);

/// Cache the last successful request
///
/// The cache expiration behavior is specified by [`CACHE_EXPIRATION`] and is calculated based on
/// the field [`DataFrame::created_instant`].
static CACHED_FRAME: Mutex<Option<DataFrame>> = Mutex::new(None);


/// Data frame capturing the queue information for one specific queue in the town hall
#[derive(Debug,Clone)]
struct QueueDataFrame {
    /// Number of people waiting in line ("Wartende Personen").
    people_waiting: u64,

    /// Last called ticket ("Aktuelle Aufrufnummer").
    last_called_ticket: i64,

    /// Waiting time estimation in minutes ("Durchschnittliche Wartezeit").
    waiting_time_estimation: u64,
}

/// Data frame containing all information at a specific point in time
#[derive(Debug,Clone)]
struct DataFrame {
    /// Data frame for citizen services ("Bürgerservice").
    citizen_services: QueueDataFrame,

    /// Data frame for drivers-license services ("Fahrerlaubnisangelegenheiten").
    drivers_license_services: QueueDataFrame,

    /// Whether this data frame is part of the [cache](CACHED_FRAME).
    cached: bool,

    /// How long it took to scrape the data.
    scrape_duration: Duration,

    /// The [Instant] that the data frame was created (monotonic).
    created_instant: Instant,

    /// The timestamp that the data frame was created (non-monotonic), based on [UNIX_EPOCH].
    created_timestamp: Duration,
}

/// Serves queue data over http
struct Server {
    listener: TcpListener,
}

/// Http responses
enum ResponseType {
    Ok,
    BadRequest,
    NotFound,
}


impl Server {
    /// Bind the server on a specific address
    pub fn init(addr: &str) -> io::Result<Self> {
        Ok(Server {
            listener: TcpListener::bind(addr)?,
        })
    }

    /// Game-loop for the server
    pub fn run(&mut self) {
        for stream in self.listener.incoming() {
            if let Ok(stream) = stream {
                stream.set_read_timeout(Some(Duration::from_millis(500)))
                    .expect("Read timeout may not be zero");
                let _ = self.handle_connection(stream);
            }
        }
    }

    /// Serve a request
    fn handle_connection(&self, stream: TcpStream) -> io::Result<()> {
        let reader = BufReader::new(&stream);
        let request_line = match reader.lines().next() {
            Some(line) => line?,
            None => return Ok(()),
        };

        let request_tokens: Vec<_> = request_line.split(' ').collect();


        if request_tokens.len() != 3 {
            Self::send_response(stream, ResponseType::BadRequest, HashMap::new(), None)
        } else if request_tokens[0] != "GET" {
            Self::send_response(stream, ResponseType::NotFound, HashMap::new(), None)
        } else {
            let path = request_tokens[1];

            if path == "/metrics" {
                match metrics() {
                    Ok(response) => Self::send_response(stream, ResponseType::Ok, HashMap::new(), Some(&response)),
                    Err(e) => {
                        eprintln!("Error: {}", e);
                        Self::send_response(stream, ResponseType::NotFound, HashMap::new(), None)
                    },
                }
            } else {
                Self::send_response(stream, ResponseType::NotFound, HashMap::new(), None)
            }
        }
    }

    /// Send a response to the client
    fn send_response(mut stream: TcpStream, response_type: ResponseType,
                        headers: HashMap<&str, &str>, content: Option<&str>) -> io::Result<()> {
        use ResponseType::*;

        let code_and_reason = match response_type {
            Ok => "200 OK",
            BadRequest => "400 BAD REQUEST",
            NotFound => "404 NOT FOUND",
        };

        let content = match content {
            Some(content) => content,
            None => code_and_reason,
        };
        let length = content.len();

        // Status line
        write!(stream, "{HTTP_VERSION} {code_and_reason}\r\n")?;

        // Headers
        for (key, value) in &headers {
            write!(stream, "{key}: {value}\r\n")?;
        }
        write!(stream, "Content-Length: {length}\r\n\r\n")?;

        // Content
        write!(stream, "{content}")?;

        stream.flush()
    }
}


/// Scrape new information from the town-hall website
fn scrape() -> Result<DataFrame, String> {
    let start = Instant::now();
    let response = reqwest::blocking::get(URL)
        .map_err(|e| e.to_string())?
        .text()
        .map_err(|e| e.to_string())?;
    let document = scraper::Html::parse_document(&response);

    let block_selector = scraper::Selector::parse(BLOCK_SELECTOR)
        .map_err(|e| e.to_string())?;
    let line_selector = scraper::Selector::parse(VALUE_SELECTOR)
        .map_err(|e| e.to_string())?;

    let blocks = document.select(&block_selector)
        .filter(|b| b.inner_html().contains(BLOCK_CONTENT_FILTER));

    let mut data_frames = Vec::new();
    for block in blocks {
        let values: Vec<_> = block.select(&line_selector)
            .map(|e| e.inner_html())
            .collect();
        if values.len() < 3 {
            return Err(String::from("not enough lines"));
        }

        let people_waiting = str::parse(&values[0])
            .map_err(|_| String::from("cannot parse waiting persons"))?;
        let last_called_ticket = str::parse(&values[1].replace(&['B', 'F'][..], ""))
            .map_err(|_| String::from("cannot parse current number"))?;
        let waiting_time_estimation = str::parse(&values[2].strip_suffix(" Minuten").unwrap_or(&values[2]))
            .map_err(|_| String::from("cannot parse waiting-time estimation"))?;

        data_frames.push(QueueDataFrame { people_waiting, last_called_ticket, waiting_time_estimation });
    }
    if data_frames.len() < 2 {
        return Err(String::from("not enough data blocks"));
    }

    Ok(DataFrame {
        citizen_services: data_frames[0].clone(),
        drivers_license_services: data_frames[1].clone(),
        scrape_duration: time::Instant::now() - start,
        cached: false,
        created_instant: Instant::now(),
        created_timestamp: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::new(0, 0)),
    })
}

/// Create a metrics string in the [Prometheus data format](https://prometheus.io/docs/instrumenting/writing_exporters/).
///
/// Metrics are taken either from [cache](CACHED_FRAME) or are [freshly scraped](scrape).
fn metrics() -> Result<String, String> {
    let mut cache = CACHED_FRAME.lock().unwrap();
    let data = if cache.is_some() && cache.clone().unwrap().created_instant > Instant::now() - CACHE_EXPIRATION {
        cache.clone().unwrap()
    } else {
        let data = scrape()?;
        cache.insert(data.clone())
            .cached = true;
        data
    };

    let mut response = String::new();

    response.push_str("# Information on the citizen service\n");
    response.push_str(&format!("erth_people_waiting{{service=\"citizen\"}}\t\t{}\n", data.citizen_services.people_waiting));
    response.push_str(&format!("erth_last_called_ticket{{service=\"citizen\"}}\t{}\n", data.citizen_services.last_called_ticket));
    response.push_str(&format!("erth_waiting_time{{service=\"citizen\"}}\t\t{}\n", data.citizen_services.waiting_time_estimation));

    response.push_str("\n# Information on the drivers-license service\n");
    response.push_str(&format!("erth_people_waiting{{service=\"drivers_license\"}}\t\t{}\n", data.drivers_license_services.people_waiting));
    response.push_str(&format!("erth_last_called_ticket{{service=\"drivers_license\"}}\t{}\n", data.drivers_license_services.last_called_ticket));
    response.push_str(&format!("erth_waiting_time{{service=\"drivers_license\"}}\t\t{}\n", data.drivers_license_services.waiting_time_estimation));


    response.push_str("\n# Meta information\n");
    response.push_str(&format!("erth_cached\t\t{}\n", data.cached as i64));
    response.push_str(&format!("erth_scrape_duration\t{}\n", data.scrape_duration.as_millis()));
    response.push_str(&format!("erth_scrape_timestamp\t{}\n", data.created_timestamp.as_millis()));

    Ok(response)
}

fn main() {
    let mut server = Server::init("localhost:12080").unwrap();
    server.run();
}

use anyhow::Result;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::{Html, IntoResponse},
    routing::get,
    Router,
};
use axum_server::bind;
use chrono::{DateTime, Duration, TimeZone, Utc};
use futures::StreamExt;
use image::{DynamicImage, ImageFormat, RgbImage};
use plotters::{prelude::*, style::full_palette::GREEN_800};
use serde::Deserialize;
use starlink_grpc_client::client::DishClient;
use starlink_grpc_client::space_x::api::device::response::Response as ResponseOneof;
use std::{collections::VecDeque, fs, io::Cursor, net::SocketAddr, sync::Arc};
use tokio::sync::{broadcast, Mutex};

// Type aliases to reduce complexity
type DataPoint = (DateTime<Utc>, f64);
type ChartHistory = VecDeque<DataPoint>;

const INDEX_HTML: &str = include_str!("../static/index.html");

#[derive(Clone)]
struct AppState {
    tx: broadcast::Sender<ChartMessage>,
    down_history: Arc<Mutex<ChartHistory>>,
    up_history: Arc<Mutex<ChartHistory>>,
    ping_history: Arc<Mutex<ChartHistory>>,
}

#[derive(Clone)]
enum ChartMessage {
    Downlink(Vec<u8>),
    Uplink(Vec<u8>),
    Ping(Vec<u8>),
}

#[derive(Deserialize)]
struct Config {
    grpc_endpoint: String,
    history_capacity: usize,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Load config
    let cfg_str = fs::read_to_string("config.toml")?;
    let config: Config = toml::from_str(&cfg_str)?;

    // Shared state: channel + histories
    let (tx, _) = broadcast::channel(16);
    let down_history = Arc::new(Mutex::new(ChartHistory::new()));
    let ping_history = Arc::new(Mutex::new(ChartHistory::new()));
    let up_history = Arc::new(Mutex::new(ChartHistory::new()));
    let state = AppState {
        tx: tx.clone(),
        down_history: down_history.clone(),
        up_history: up_history.clone(),
        ping_history: ping_history.clone(),
    };
    let history_capacity = config.history_capacity;

    // Pre-populate initial data for first load
    {
        let now = Utc::now();
        let mut uh = up_history.lock().await;
        let mut dh = down_history.lock().await;
        let mut ph = ping_history.lock().await;
        for n in 0..history_capacity {
            uh.push_back((now - Duration::seconds((history_capacity - n) as i64), 0.0));
            dh.push_back((now - Duration::seconds((history_capacity - n) as i64), 0.0));
            ph.push_back((now - Duration::seconds((history_capacity - n) as i64), 25.0));
        }
    }

    // Spawn gRPC stream data generator
    {
        let down_history = down_history.clone();
        let up_history = up_history.clone();
        let tx = tx.clone();
        let endpoint = config.grpc_endpoint.clone();

        tokio::spawn(async move {
            loop {
                eprintln!("Connecting to Dish endpoint: {}", endpoint);
                match DishClient::connect(&endpoint).await {
                    Ok(mut client) => {
                        eprintln!("Connected to Dish endpoint");
                        eprintln!("Opening gRPC status stream");
                        match client.stream_status().await {
                            Ok(mut stream) => {
                                eprintln!("Status stream opened");
                                while let Some(item) = stream.next().await {
                                    // eprintln!("Waiting for the next message {}", Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
                                    match item {
                                        Ok(status) => {
                                            if let Some(ResponseOneof::DishGetStatus(dgs)) =
                                                status.raw.response
                                            {
                                                let down_val = dgs.downlink_throughput_bps as f64
                                                    / 1_000_000.0;
                                                let up_val =
                                                    dgs.uplink_throughput_bps as f64 / 1_000_000.0;
                                                let now = Utc::now();
                                                // eprintln!("Received message: UP: {up_val:.2}, DOWN: {down_val:.2}");
                                                // update histories
                                                {
                                                    let mut hist = down_history.lock().await;
                                                    hist.push_back((now, down_val));
                                                    if hist.len() > history_capacity {
                                                        hist.pop_front();
                                                    }
                                                }
                                                {
                                                    let mut hist = up_history.lock().await;
                                                    hist.push_back((now, up_val));
                                                    let ping_val = dgs.pop_ping_latency_ms as f64;
                                                    let mut phist = ping_history.lock().await;
                                                    phist.push_back((now, ping_val));
                                                    if phist.len() > history_capacity {
                                                        phist.pop_front();
                                                    }
                                                    if hist.len() > history_capacity {
                                                        hist.pop_front();
                                                    }
                                                }

                                                // render and broadcast
                                                let dh_vec = down_history.lock().await.clone();
                                                let uh_vec = up_history.lock().await.clone();
                                                let ph_vec = ping_history.lock().await.clone();

                                                if let Ok(buf) = render_png(
                                                    "Downlink Throughput",
                                                    &dh_vec,
                                                    |v| v,
                                                    "Mbps",
                                                ) {
                                                    let _ = tx.send(ChartMessage::Downlink(buf));
                                                }
                                                if let Ok(buf) = render_png(
                                                    "Uplink Throughput",
                                                    &uh_vec,
                                                    |v| v,
                                                    "Mbps",
                                                ) {
                                                    let _ = tx.send(ChartMessage::Uplink(buf));
                                                }
                                                if let Ok(buf) =
                                                    render_png("Ping Latency", &ph_vec, |v| v, "ms")
                                                {
                                                    let _ = tx.send(ChartMessage::Ping(buf));
                                                }
                                            }
                                        }
                                        Err(err) => {
                                            eprintln!("Stream error: {:?}", err);
                                        }
                                    }
                                }
                            }
                            Err(err) => {
                                eprintln!("Failed to open status stream: {:?}", err);
                            }
                        }
                    }
                    Err(err) => {
                        eprintln!("Connection error: {:?}", err);
                    }
                }
                eprintln!("Reconnecting to dish in 5 seconds...");
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        });
    }
    // Build routes
    let app = Router::new()
        .route("/", get(index))
        .route("/ws", get(ws_handler))
        .route("/initial/down", get(initial_down))
        .route("/initial/up", get(initial_up))
        .route("/initial/ping", get(initial_ping))
        .with_state(state);

    let addr: SocketAddr = "0.0.0.0:8080".parse().unwrap();
    println!("Open your browser and navigate to http://localhost:8080");
    bind(addr).serve(app.into_make_service()).await?;
    Ok(())
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn initial_down(State(state): State<AppState>) -> impl IntoResponse {
    let data = state.down_history.lock().await.clone();
    if let Ok(png) = render_png("Downlink Throughput", &data, |v| v, "Mbps") {
        ([("Content-Type", "image/png")], png)
    } else {
        ([("Content-Type", "text/plain")], Vec::new())
    }
}
async fn initial_up(State(state): State<AppState>) -> impl IntoResponse {
    let data = state.up_history.lock().await.clone();
    if let Ok(png) = render_png("Uplink Throughput", &data, |v| v, "Mbps") {
        ([("Content-Type", "image/png")], png)
    } else {
        ([("Content-Type", "text/plain")], Vec::new())
    }
}

async fn initial_ping(State(state): State<AppState>) -> impl IntoResponse {
    let data = state.ping_history.lock().await.clone();
    if let Ok(png) = render_png("Ping Latency", &data, |v| v, "ms") {
        ([("Content-Type", "image/png")], png)
    } else {
        ([("Content-Type", "text/plain")], Vec::new())
    }
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws(socket, state.tx.clone()))
}

async fn handle_ws(mut socket: WebSocket, tx: broadcast::Sender<ChartMessage>) {
    let mut rx = tx.subscribe();
    while let Ok(msg) = rx.recv().await {
        let mut data = Vec::new();
        match msg {
            ChartMessage::Downlink(buf) => {
                data.push(0);
                data.extend(buf);
            }
            ChartMessage::Uplink(buf) => {
                data.push(1);
                data.extend(buf);
            }
            ChartMessage::Ping(buf) => {
                data.push(2);
                data.extend(buf);
            }
        }
        if socket.send(Message::Binary(data.into())).await.is_err() {
            break;
        }
    }
}

fn render_png<F>(
    title: &str,
    data: &ChartHistory,
    transform: F,
    y_desc: &str,
) -> Result<Vec<u8>, Box<dyn std::error::Error>>
where
    F: Fn(f64) -> f64,
{
    let width = 1200u32;
    let height = 300u32;
    let mut raw = vec![0u8; (width * height * 3) as usize];
    {
        let backend = BitMapBackend::with_buffer(&mut raw, (width, height));
        let root = backend.into_drawing_area();
        root.fill(&WHITE)?;
        let x_min = data.front().map(|(t, _)| t.timestamp_millis()).unwrap_or(0);
        let x_max = data.back().map(|(t, _)| t.timestamp_millis()).unwrap_or(0);
        let ys: Vec<f64> = data.iter().map(|(_, v)| transform(*v)).collect();
        let y_min = ys.iter().cloned().fold(f64::INFINITY, f64::min);
        let y_max = ys
            .iter()
            .cloned()
            .fold(f64::NEG_INFINITY, f64::max)
            .max(0.0);
        let mut chart = ChartBuilder::on(&root)
            .caption(title, ("sans-serif", 20).into_font())
            .margin(10)
            .x_label_area_size(30)
            .y_label_area_size(40)
            .build_cartesian_2d(x_min..x_max, y_min..y_max)?;
        chart
            .configure_mesh()
            .max_light_lines(1)
            .x_desc("Time")
            .y_desc(y_desc)
            .x_labels(5)
            .x_label_formatter(&|x| {
                // x is milliseconds since epoch
                let timestamp = *x;
                let secs = timestamp / 1000;
                let nsecs = ((timestamp % 1000) * 1_000_000) as u32;
                // Convert to DateTime<Utc>
                let dt = Utc
                    .timestamp_opt(secs, nsecs)
                    .single()
                    .unwrap_or_else(|| Utc.timestamp_opt(0, 0).single().unwrap());
                dt.format("%H:%M:%S").to_string()
            })
            .draw()?;
        chart.draw_series(LineSeries::new(
            data.iter()
                .map(|(t, v)| (t.timestamp_millis(), transform(*v))),
            &GREEN_800,
        ))?;
        root.present()?;
    }
    // Encode raw buffer to PNG
    let img = RgbImage::from_raw(width, height, raw).ok_or("Buffer->Image failed")?;
    let dyn_img = DynamicImage::ImageRgb8(img);
    let mut cursor = Cursor::new(Vec::new());
    dyn_img.write_to(&mut cursor, ImageFormat::Png)?;
    Ok(cursor.into_inner())
}

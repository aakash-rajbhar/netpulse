use std::{
    error::Error as StdError,
    io::{self, Read},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    symbols::Marker,
    text::{Line, Span},
    widgets::{
        Axis, Block, BorderType, Borders, Cell, Chart, Dataset, GraphType, Paragraph, Row, Table,
        Wrap,
    },
};
use reqwest::{
    blocking::Client,
    header::{CONTENT_TYPE, HeaderValue, ORIGIN, REFERER},
};
use rusqlite::{Connection, params};

// ───────────────────────────── Config ─────────────────────────────

const LATENCY_SAMPLES: usize = 5;
const DOWNLOAD_BYTES: usize = 10_000_000; // per request; repeated until the deadline
const DOWNLOAD_DEADLINE: Duration = Duration::from_secs(8);

const UPLOAD_URL: &str = "https://speed.cloudflare.com/__up";
const UPLOAD_CHUNK_BYTES: usize = 512 * 1024;
const UPLOAD_MAX_BYTES: usize = 128 * 1024 * 1024;
const UPLOAD_DEADLINE: Duration = Duration::from_secs(8);

// ───────────────────────────── Palette ─────────────────────────────

type Rgb = (u8, u8, u8);

const CYAN: Rgb = (80, 200, 255);
const GREEN: Rgb = (80, 230, 140);
const BLUE: Rgb = (100, 150, 255);
const YELLOW: Rgb = (255, 205, 70);
const MAGENTA: Rgb = (215, 110, 255);
const RED: Rgb = (255, 95, 95);
const GOLD: Rgb = (255, 214, 90);
const WHITE: Rgb = (255, 255, 255);
const MUTED: Rgb = (110, 116, 138);
const TRACK: Rgb = (45, 50, 66);

fn rgb(c: Rgb) -> Color {
    Color::Rgb(c.0, c.1, c.2)
}

fn lerp(a: Rgb, b: Rgb, t: f64) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    let f = |x: u8, y: u8| (x as f64 + (y as f64 - x as f64) * t).round() as u8;
    (f(a.0, b.0), f(a.1, b.1), f(a.2, b.2))
}

fn dim(c: Rgb, k: f64) -> Rgb {
    lerp((0, 0, 0), c, k)
}

fn fg(c: Rgb) -> Style {
    Style::default().fg(rgb(c))
}

fn bold(c: Rgb) -> Style {
    fg(c).add_modifier(Modifier::BOLD)
}

// ───────────────────────────── Data types ─────────────────────────────

struct TestRecord {
    timestamp: String,
    download: Option<f64>,
    upload: Option<f64>,
    latency: Option<f64>,
    jitter: Option<f64>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Latency,
    Download,
    Upload,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Dashboard,
    History,
}

enum TestUpdate {
    Phase(Phase),
    LatencySample(f64),
    LatencySummary {
        average: Option<f64>,
        jitter: Option<f64>,
        error: Option<String>,
    },
    /// Live throughput sample while a download/upload is running.
    Live {
        kind: Phase,
        t: f64,
        mbps: f64,
        frac: f64,
    },
    Download(Result<f64, String>),
    Upload(Result<f64, String>),
    Finished,
}

/// A number that eases toward its target so values "count up" smoothly.
#[derive(Default)]
struct Meter {
    target: Option<f64>,
    shown: f64,
}

impl Meter {
    fn tick(&mut self) {
        match self.target {
            Some(target) => {
                self.shown += (target - self.shown) * 0.22;
                if (target - self.shown).abs() < 0.005 {
                    self.shown = target;
                }
            }
            None => self.shown = 0.0,
        }
    }

    fn value(&self) -> Option<f64> {
        self.target.map(|_| self.shown)
    }
}

struct App {
    started: Instant,
    tick: usize,

    phase: Phase,
    phase_started: Instant,
    phase_frac: f64,
    view: View,
    status: String,
    ran: bool,
    test_started: Option<Instant>,
    elapsed_final: u64,
    done_at: Option<Instant>,
    shown_progress: f64,

    down: Meter,
    up: Meter,
    lat: Meter,
    jit: Meter,
    samples: Vec<f64>,
    live_down: Vec<(f64, f64)>,
    live_up: Vec<(f64, f64)>,

    latency_err: Option<String>,
    download_err: Option<String>,
    upload_err: Option<String>,

    history: Vec<TestRecord>,
}

impl App {
    fn new(history: Vec<TestRecord>) -> Self {
        Self {
            started: Instant::now(),
            tick: 0,
            phase: Phase::Idle,
            phase_started: Instant::now(),
            phase_frac: 0.0,
            view: View::Dashboard,
            status: String::new(),
            ran: false,
            test_started: None,
            elapsed_final: 0,
            done_at: None,
            shown_progress: 0.0,
            down: Meter::default(),
            up: Meter::default(),
            lat: Meter::default(),
            jit: Meter::default(),
            samples: Vec::new(),
            live_down: Vec::new(),
            live_up: Vec::new(),
            latency_err: None,
            download_err: None,
            upload_err: None,
            history,
        }
    }

    fn is_testing(&self) -> bool {
        self.phase != Phase::Idle
    }

    fn has_results(&self) -> bool {
        self.down.target.is_some() || self.up.target.is_some() || self.lat.target.is_some()
    }

    fn has_errors(&self) -> bool {
        self.latency_err.is_some() || self.download_err.is_some() || self.upload_err.is_some()
    }

    fn reset_for_test(&mut self) {
        self.down = Meter::default();
        self.up = Meter::default();
        self.lat = Meter::default();
        self.jit = Meter::default();
        self.samples.clear();
        self.live_down.clear();
        self.live_up.clear();
        self.latency_err = None;
        self.download_err = None;
        self.upload_err = None;
        self.view = View::Dashboard;
        self.ran = true;
        self.phase = Phase::Latency;
        self.phase_started = Instant::now();
        self.phase_frac = 0.0;
        self.test_started = Some(Instant::now());
        self.done_at = None;
        self.shown_progress = 0.0;
        self.status.clear();
    }

    fn target_progress(&self) -> f64 {
        let frac = self.phase_frac.clamp(0.0, 1.0);
        match self.phase {
            Phase::Idle => {
                if self.ran {
                    100.0
                } else {
                    0.0
                }
            }
            Phase::Latency => self.samples.len() as f64 * 30.0 / LATENCY_SAMPLES as f64,
            Phase::Download => 30.0 + 35.0 * frac,
            Phase::Upload => 65.0 + 34.0 * frac,
        }
    }

    /// Called once per frame: advances time-based animation state.
    fn update_frame(&mut self) {
        self.tick = (self.started.elapsed().as_millis() / 50) as usize;

        self.down.tick();
        self.up.tick();
        self.lat.tick();
        self.jit.tick();

        let target = self.target_progress();
        if target < self.shown_progress {
            self.shown_progress = target;
        } else {
            self.shown_progress += (target - self.shown_progress) * 0.2;
        }
    }

    fn flashing(&self) -> bool {
        self.done_at
            .is_some_and(|t| t.elapsed() < Duration::from_millis(1500) && (self.tick / 3) % 2 == 0)
    }

    fn elapsed_secs(&self) -> u64 {
        if self.is_testing() {
            self.test_started.map_or(0, |t| t.elapsed().as_secs())
        } else {
            self.elapsed_final
        }
    }
}

// ───────────────────────────── Database ─────────────────────────────

fn init_database() -> rusqlite::Result<Connection> {
    let connection = Connection::open("netpulse.db")?;

    connection.execute(
        "CREATE TABLE IF NOT EXISTS test_history (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            tested_at TEXT NOT NULL DEFAULT (
                datetime('now', 'localtime')
            ),
            download_mbps REAL,
            upload_mbps REAL,
            latency_ms REAL,
            jitter_ms REAL
        )",
        [],
    )?;

    Ok(connection)
}

fn save_test(
    connection: &Connection,
    download: Option<f64>,
    upload: Option<f64>,
    latency: Option<f64>,
    jitter: Option<f64>,
) -> rusqlite::Result<()> {
    connection.execute(
        "INSERT INTO test_history (
            download_mbps,
            upload_mbps,
            latency_ms,
            jitter_ms
        ) VALUES (?1, ?2, ?3, ?4)",
        params![download, upload, latency, jitter],
    )?;

    Ok(())
}

fn load_history(connection: &Connection) -> rusqlite::Result<Vec<TestRecord>> {
    let mut statement = connection.prepare(
        "SELECT
            tested_at,
            download_mbps,
            upload_mbps,
            latency_ms,
            jitter_ms
         FROM test_history
         ORDER BY id DESC
         LIMIT 10",
    )?;

    let records = statement
        .query_map([], |row| {
            Ok(TestRecord {
                timestamp: row.get(0)?,
                download: row.get(1)?,
                upload: row.get(2)?,
                latency: row.get(3)?,
                jitter: row.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<TestRecord>>>()?;

    Ok(records)
}

// ───────────────────────────── Measurements ─────────────────────────────

/// Builds a readable error message including the underlying cause chain.
fn describe(error: &reqwest::Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();

    while let Some(cause) = source {
        message.push_str(&format!(": {cause}"));
        source = cause.source();
    }

    message
}

const USER_AGENT_STR: &str = "Mozilla/5.0 (X11; Linux x86_64) NetPulse/0.1";

fn build_client() -> Result<Client, reqwest::Error> {
    Client::builder()
        .user_agent(USER_AGENT_STR)
        .timeout(Duration::from_secs(60))
        .build()
}

fn measure_latency(client: &Client) -> Result<f64, String> {
    let start = Instant::now();

    client
        .get("https://www.google.com/generate_204")
        .send()
        .and_then(|response| response.error_for_status())
        .map_err(|e| describe(&e))?;

    Ok(start.elapsed().as_secs_f64() * 1000.0)
}

/// Streams downloads (repeating 10 MB requests) until the deadline, emitting
/// live throughput samples roughly every 100 ms.
fn measure_download(
    client: &Client,
    tx: &Sender<TestUpdate>,
    test_start: Instant,
) -> Result<f64, String> {
    let start = Instant::now();
    let mut total = 0usize;
    let mut window_bytes = 0usize;
    let mut window_start = Instant::now();
    let mut buffer = vec![0u8; 64 * 1024];

    'requests: loop {
        let response = client
            .get(format!(
                "https://speed.cloudflare.com/__down?bytes={DOWNLOAD_BYTES}"
            ))
            .header(
                ORIGIN,
                HeaderValue::from_static("https://speed.cloudflare.com"),
            )
            .header(
                REFERER,
                HeaderValue::from_static("https://speed.cloudflare.com/"),
            )
            .send()
            .and_then(|response| response.error_for_status());

        let mut response = match response {
            Ok(response) => response,
            Err(error) => {
                if total == 0 {
                    return Err(format!("download: {}", describe(&error)));
                }
                break;
            }
        };

        loop {
            let read = match response.read(&mut buffer) {
                Ok(read) => read,
                Err(error) => {
                    if total == 0 {
                        return Err(format!("download: {error}"));
                    }
                    break 'requests;
                }
            };

            if read == 0 {
                break;
            }

            total += read;
            window_bytes += read;

            let window_secs = window_start.elapsed().as_secs_f64();
            if window_secs >= 0.1 {
                let _ = tx.send(TestUpdate::Live {
                    kind: Phase::Download,
                    t: test_start.elapsed().as_secs_f64(),
                    mbps: window_bytes as f64 * 8.0 / (window_secs * 1_000_000.0),
                    frac: start.elapsed().as_secs_f64() / DOWNLOAD_DEADLINE.as_secs_f64(),
                });
                window_bytes = 0;
                window_start = Instant::now();
            }

            if start.elapsed() >= DOWNLOAD_DEADLINE {
                break 'requests;
            }
        }
    }

    let seconds = start.elapsed().as_secs_f64();

    if total == 0 || seconds <= 0.0 {
        return Err("download: invalid measurement".into());
    }

    Ok(total as f64 * 8.0 / (seconds * 1_000_000.0))
}

/// Uploads in small chunks (like the browser speed test does) with a short
/// per-request timeout. Partial progress still counts.
fn upload_chunks(
    client: &Client,
    tx: &Sender<TestUpdate>,
    test_start: Instant,
) -> Result<f64, String> {
    let payload = vec![b'0'; UPLOAD_CHUNK_BYTES];
    let start = Instant::now();
    let mut last_ok = start;
    let mut sent = 0usize;
    let mut last_error: Option<String> = None;

    while sent < UPLOAD_MAX_BYTES && start.elapsed() < UPLOAD_DEADLINE {
        let chunk_start = Instant::now();

        let result = client
            .post(UPLOAD_URL)
            .header(
                ORIGIN,
                HeaderValue::from_static("https://speed.cloudflare.com"),
            )
            .header(
                REFERER,
                HeaderValue::from_static("https://speed.cloudflare.com/"),
            )
            .header(CONTENT_TYPE, "text/plain;charset=UTF-8")
            .timeout(Duration::from_secs(10))
            .body(payload.clone())
            .send()
            .and_then(|response| response.error_for_status())
            .and_then(|response| response.bytes());

        match result {
            Ok(_) => {
                let now = Instant::now();
                let chunk_secs = now.duration_since(chunk_start).as_secs_f64().max(1e-6);

                sent += UPLOAD_CHUNK_BYTES;
                last_ok = now;

                let _ = tx.send(TestUpdate::Live {
                    kind: Phase::Upload,
                    t: test_start.elapsed().as_secs_f64(),
                    mbps: UPLOAD_CHUNK_BYTES as f64 * 8.0 / (chunk_secs * 1_000_000.0),
                    frac: start.elapsed().as_secs_f64() / UPLOAD_DEADLINE.as_secs_f64(),
                });
            }
            Err(error) => {
                last_error = Some(describe(&error));
                break;
            }
        }
    }

    if sent == 0 {
        return Err(format!(
            "first {} KiB chunk failed: {}",
            UPLOAD_CHUNK_BYTES / 1024,
            last_error.unwrap_or_else(|| "timed out".into())
        ));
    }

    let seconds = last_ok.duration_since(start).as_secs_f64();

    if seconds <= 0.0 {
        return Err("upload: invalid duration".into());
    }

    Ok((sent as f64 * 8.0) / (seconds * 1_000_000.0))
}

fn measure_upload(
    client: &Client,
    tx: &Sender<TestUpdate>,
    test_start: Instant,
) -> Result<f64, String> {
    let first = match upload_chunks(client, tx, test_start) {
        Ok(value) => return Ok(value),
        Err(error) => error,
    };

    // Retry once over HTTP/1.1 only; some networks/proxies stall HTTP/2 uploads.
    let http1 = Client::builder()
        .user_agent(USER_AGENT_STR)
        .http1_only()
        .build()
        .map_err(|e| describe(&e))?;

    upload_chunks(&http1, tx, test_start)
        .map_err(|second| format!("HTTP/2: {first}  |  HTTP/1.1: {second}"))
}

fn start_test(client: Client, tx: Sender<TestUpdate>) {
    thread::spawn(move || {
        let test_start = Instant::now();

        // ── Latency ──
        let _ = tx.send(TestUpdate::Phase(Phase::Latency));

        let mut samples = Vec::with_capacity(LATENCY_SAMPLES);
        let mut last_error: Option<String> = None;

        for index in 1..=LATENCY_SAMPLES {
            match measure_latency(&client) {
                Ok(value) => {
                    samples.push(value);
                    let _ = tx.send(TestUpdate::LatencySample(value));
                }
                Err(error) => last_error = Some(format!("latency: {error}")),
            }

            if index < LATENCY_SAMPLES {
                thread::sleep(Duration::from_millis(250));
            }
        }

        let average = if samples.is_empty() {
            None
        } else {
            Some(samples.iter().sum::<f64>() / samples.len() as f64)
        };

        let jitter = if samples.len() < 2 {
            None
        } else {
            let differences: f64 = samples
                .windows(2)
                .map(|pair| (pair[1] - pair[0]).abs())
                .sum();

            Some(differences / (samples.len() - 1) as f64)
        };

        let error = if average.is_none() { last_error } else { None };
        let _ = tx.send(TestUpdate::LatencySummary {
            average,
            jitter,
            error,
        });

        // ── Download ──
        let _ = tx.send(TestUpdate::Phase(Phase::Download));
        let _ = tx.send(TestUpdate::Download(measure_download(
            &client, &tx, test_start,
        )));

        // ── Upload ──
        let _ = tx.send(TestUpdate::Phase(Phase::Upload));
        let _ = tx.send(TestUpdate::Upload(measure_upload(&client, &tx, test_start)));

        let _ = tx.send(TestUpdate::Finished);
    });
}

// ───────────────────────────── State updates ─────────────────────────────

fn apply_update(app: &mut App, update: TestUpdate, connection: &Connection) {
    match update {
        TestUpdate::Phase(phase) => {
            app.phase = phase;
            app.phase_started = Instant::now();
            app.phase_frac = 0.0;
        }

        TestUpdate::LatencySample(value) => app.samples.push(value),

        TestUpdate::LatencySummary {
            average,
            jitter,
            error,
        } => {
            app.lat.target = average;
            app.jit.target = jitter;
            app.latency_err = error;
        }

        TestUpdate::Live {
            kind,
            t,
            mbps,
            frac,
        } => {
            app.phase_frac = frac;

            // Light smoothing so the graph looks fluid rather than jittery.
            match kind {
                Phase::Download => {
                    let smooth = app
                        .live_down
                        .last()
                        .map_or(mbps, |&(_, prev)| prev * 0.5 + mbps * 0.5);
                    app.live_down.push((t, smooth));
                    app.down.target = Some(smooth);
                }
                Phase::Upload => {
                    let smooth = app
                        .live_up
                        .last()
                        .map_or(mbps, |&(_, prev)| prev * 0.5 + mbps * 0.5);
                    app.live_up.push((t, smooth));
                    app.up.target = Some(smooth);
                }
                _ => {}
            }
        }

        TestUpdate::Download(result) => match result {
            Ok(value) => app.down.target = Some(value),
            Err(error) => {
                app.down.target = None;
                app.download_err = Some(error);
            }
        },

        TestUpdate::Upload(result) => match result {
            Ok(value) => app.up.target = Some(value),
            Err(error) => {
                app.up.target = None;
                app.upload_err = Some(error);
            }
        },

        TestUpdate::Finished => {
            app.elapsed_final = app.test_started.map_or(0, |t| t.elapsed().as_secs());
            app.phase = Phase::Idle;
            app.done_at = Some(Instant::now());

            // Failed metrics are stored as NULL.
            let saved = save_test(
                connection,
                app.down.target,
                app.up.target,
                app.lat.target,
                app.jit.target,
            );

            app.status = match saved {
                Ok(()) => {
                    app.history = load_history(connection).unwrap_or_default();

                    let failures = [
                        app.latency_err.is_some(),
                        app.download_err.is_some(),
                        app.upload_err.is_some(),
                    ]
                    .iter()
                    .filter(|failed| **failed)
                    .count();

                    match failures {
                        0 => "Test complete — saved to history".into(),
                        3 => "All tests failed — attempt saved".into(),
                        n => format!("Finished with {n} error(s) — attempt saved"),
                    }
                }
                Err(error) => format!("History save failed: {error}"),
            };
        }
    }
}

// ───────────────────────────── Visual helpers ─────────────────────────────

fn phase_color(app: &App) -> Rgb {
    match app.phase {
        Phase::Latency => YELLOW,
        Phase::Download => GREEN,
        Phase::Upload => BLUE,
        Phase::Idle => {
            if !app.ran {
                CYAN
            } else if app.has_errors() {
                YELLOW
            } else {
                GREEN
            }
        }
    }
}

/// Per-character gradient from `from` to `to`, with a bright shine sweeping across.
fn shimmer(text: &str, from: Rgb, to: Rgb, tick: usize, bold_text: bool) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len().max(1);
    let period = len + 14;
    let center = (tick % period) as f64 - 7.0;

    chars
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let base = lerp(from, to, i as f64 / len as f64);
            let boost = (1.0 - (i as f64 - center).abs() / 5.0).max(0.0);
            let color = lerp(base, WHITE, boost * 0.85);

            let mut style = fg(color);
            if bold_text {
                style = style.add_modifier(Modifier::BOLD);
            }

            Span::styled(c.to_string(), style)
        })
        .collect()
}

/// A smooth (1/8th-block precision) gradient progress bar with a moving shine.
fn fancy_bar(width: usize, frac: f64, base: Rgb, tick: usize, shine: bool) -> Vec<Span<'static>> {
    const PARTIAL: [&str; 8] = [" ", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];

    let frac = frac.clamp(0.0, 1.0);
    let fill = frac * width as f64;
    let full = fill.floor() as usize;
    let part = (((fill - full as f64) * 8.0) as usize).min(7);
    let center = if shine && full > 0 {
        (tick % (full + 10)) as f64 - 5.0
    } else {
        -100.0
    };

    let mut spans = Vec::with_capacity(width);

    for i in 0..width {
        if i < full {
            let t = i as f64 / full.max(1) as f64;
            let color = lerp(dim(base, 0.35), base, t);
            let boost = (1.0 - (i as f64 - center).abs() / 4.0).max(0.0);
            spans.push(Span::styled("█", fg(lerp(color, WHITE, boost * 0.8))));
        } else if i == full && part > 0 {
            spans.push(Span::styled(PARTIAL[part], fg(base)));
        } else {
            spans.push(Span::styled("░", fg(TRACK)));
        }
    }

    spans
}

fn big_glyph(c: char) -> [&'static str; 3] {
    match c {
        '0' => ["┏━┓", "┃ ┃", "┗━┛"],
        '1' => [" ╻ ", " ┃ ", " ╹ "],
        '2' => ["╺━┓", "┏━┛", "┗━╸"],
        '3' => ["╺━┓", "╺━┫", "╺━┛"],
        '4' => ["╻ ╻", "┗━┫", "  ╹"],
        '5' => ["┏━╸", "┗━┓", "╺━┛"],
        '6' => ["┏━╸", "┣━┓", "┗━┛"],
        '7' => ["╺━┓", "  ┃", "  ╹"],
        '8' => ["┏━┓", "┣━┫", "┗━┛"],
        '9' => ["┏━┓", "┗━┫", "╺━┛"],
        '.' => [" ", " ", "▪"],
        _ => ["   ", "   ", "   "],
    }
}

fn big_text(text: &str) -> [String; 3] {
    let mut rows = [String::new(), String::new(), String::new()];

    for (index, c) in text.chars().enumerate() {
        let glyph = big_glyph(c);
        for (row, part) in rows.iter_mut().zip(glyph.iter()) {
            if index > 0 {
                row.push(' ');
            }
            row.push_str(part);
        }
    }

    rows
}

fn rounded_block(title: &str, color: Color) -> Block<'static> {
    Block::default()
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color))
}

fn metric(value: Option<f64>, unit: &str) -> String {
    match value {
        Some(value) => format!("{value:.2} {unit}"),
        None => "--".into(),
    }
}

fn breath(tick: usize, speed: f64) -> f64 {
    ((tick as f64 * speed).sin() + 1.0) / 2.0
}

// ───────────────────────────── UI ─────────────────────────────

fn ui(frame: &mut Frame, app: &App) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header
            Constraint::Length(4), // status + progress
            Constraint::Min(10),   // main
            Constraint::Length(1), // key hints
        ])
        .split(frame.area());

    draw_header(frame, areas[0], app);
    draw_status(frame, areas[1], app);

    match app.view {
        View::Dashboard => draw_dashboard(frame, areas[2], app),
        View::History => draw_history(frame, areas[2], app),
    }

    draw_keys(frame, areas[3], app);
}

// A heartbeat line: tall spikes scroll past like an ECG.
const ECG: [usize; 28] = [
    1, 1, 1, 1, 2, 1, 1, 1, 1, 1, 1, 1, 3, 8, 2, 1, 1, 1, 1, 2, 3, 2, 1, 1, 1, 1, 1, 1,
];
const BLOCKS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

fn ecg_spans(width: usize, tick: usize, base: Rgb, fast: bool) -> Vec<Span<'static>> {
    let shift = if fast { tick } else { tick / 3 };

    (0..width)
        .map(|i| {
            let h = ECG[(i + shift) % ECG.len()];
            let fade = i as f64 / width.max(1) as f64;
            let color = lerp(dim(base, 0.2), base, fade);
            Span::styled(BLOCKS[h].to_string(), fg(color))
        })
        .collect()
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let color = phase_color(app);
    let inner_w = area.width.saturating_sub(2) as usize;

    let glyph = ["●", "◉", "◎", "◉"][(app.tick / 4) % 4];
    let title = "N E T P U L S E";

    let active_tab = Style::default()
        .fg(Color::Black)
        .bg(rgb(CYAN))
        .add_modifier(Modifier::BOLD);

    let (dash_style, hist_style) = match app.view {
        View::Dashboard => (active_tab, fg(MUTED)),
        View::History => (fg(MUTED), active_tab),
    };

    let mut spans = vec![
        Span::raw(" "),
        Span::styled(glyph, bold(color)),
        Span::raw(" "),
    ];
    spans.extend(shimmer(title, CYAN, MAGENTA, app.tick, true));
    spans.push(Span::raw("   "));
    spans.push(Span::styled(" Dashboard ", dash_style));
    spans.push(Span::raw(" "));
    spans.push(Span::styled(" History ", hist_style));
    spans.push(Span::raw("   "));

    let used = 3 + title.chars().count() + 3 + 11 + 1 + 9 + 3;
    let ecg_width = inner_w.saturating_sub(used + 1);

    if ecg_width >= 6 {
        spans.extend(ecg_spans(ecg_width, app.tick, color, app.is_testing()));
    }

    let header = Paragraph::new(Line::from(spans)).block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(fg(dim(color, 0.8))),
    );

    frame.render_widget(header, area);
}

const SPIN: [&str; 10] = ["·", "✢", "✳", "✶", "✻", "✽", "✻", "✶", "✳", "✢"];
const LATENCY_VERBS: [&str; 5] = [
    "Pinging the cloud",
    "Echolocating",
    "Bouncing packets",
    "Listening for echoes",
    "Poking servers",
];
const DOWNLOAD_VERBS: [&str; 5] = [
    "Slurping bytes",
    "Hauling data",
    "Reeling in packets",
    "Siphoning the internet",
    "Catching bits",
];
const UPLOAD_VERBS: [&str; 5] = [
    "Launching bytes",
    "Beaming data skyward",
    "Slinging packets",
    "Pushing data uphill",
    "Sending smoke signals",
];

fn draw_status(frame: &mut Frame, area: Rect, app: &App) {
    let color = phase_color(app);
    let inner_w = area.width.saturating_sub(2) as usize;

    let mut line1: Vec<Span> = vec![Span::raw(" ")];

    if app.is_testing() {
        let verbs: &[&str] = match app.phase {
            Phase::Latency => &LATENCY_VERBS,
            Phase::Download => &DOWNLOAD_VERBS,
            _ => &UPLOAD_VERBS,
        };
        let verb = verbs[(app.phase_started.elapsed().as_secs() / 2) as usize % verbs.len()];

        line1.push(Span::styled(SPIN[(app.tick / 2) % SPIN.len()], bold(color)));
        line1.push(Span::raw(" "));
        line1.extend(shimmer(&format!("{verb}…"), color, color, app.tick, true));
        line1.push(Span::styled(
            format!(
                "  ({}s · {:.0}%)",
                app.elapsed_secs(),
                app.shown_progress.min(100.0)
            ),
            fg(MUTED),
        ));
    } else if app.ran {
        let icon = if app.has_errors() { "▲" } else { "✔" };
        line1.push(Span::styled(icon, bold(color)));
        line1.push(Span::raw(" "));
        line1.push(Span::styled(app.status.clone(), bold(color)));
        line1.push(Span::styled(
            format!("  (took {}s)", app.elapsed_secs()),
            fg(MUTED),
        ));
    } else {
        let tone = lerp(MUTED, CYAN, breath(app.tick, 0.12));
        line1.push(Span::styled("✻", bold(tone)));
        line1.push(Span::raw(" "));
        line1.push(Span::styled(
            "Ready when you are — press R to launch a speed test",
            fg(tone),
        ));
    }

    let bar = fancy_bar(
        inner_w.saturating_sub(2),
        app.shown_progress / 100.0,
        color,
        app.tick,
        app.is_testing(),
    );
    let mut line2 = vec![Span::raw(" ")];
    line2.extend(bar);

    let panel = Paragraph::new(vec![Line::from(line1), Line::from(line2)])
        .block(rounded_block("Status", rgb(color)));

    frame.render_widget(panel, area);
}

const MBPS_SCALES: [f64; 10] = [
    10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0, 10000.0,
];

fn draw_dashboard(frame: &mut Frame, area: Rect, app: &App) {
    let show_flow = area.height >= 21;

    let rows = if show_flow {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(7),
                Constraint::Length(3),
                Constraint::Min(8),
            ])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(7), Constraint::Min(5)])
            .split(area)
    };

    let cards = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(25); 4])
        .split(rows[0]);

    let flash = app.flashing();

    let specs = [
        CardSpec {
            title: "Download",
            value: app.down.value(),
            unit: "Mbps",
            color: GREEN,
            failed: app.download_err.is_some(),
            active: app.phase == Phase::Download,
            quality_bar: false,
        },
        CardSpec {
            title: "Upload",
            value: app.up.value(),
            unit: "Mbps",
            color: BLUE,
            failed: app.upload_err.is_some(),
            active: app.phase == Phase::Upload,
            quality_bar: false,
        },
        CardSpec {
            title: "Avg Latency",
            value: app.lat.value(),
            unit: "ms",
            color: YELLOW,
            failed: app.latency_err.is_some(),
            active: app.phase == Phase::Latency,
            quality_bar: true,
        },
        CardSpec {
            title: "Jitter",
            value: app.jit.value(),
            unit: "ms",
            color: MAGENTA,
            failed: false,
            active: app.phase == Phase::Latency,
            quality_bar: true,
        },
    ];

    for (area, spec) in cards.iter().zip(specs.iter()) {
        draw_card(frame, *area, spec, app.tick, flash);
    }

    let bottom = if show_flow {
        draw_flow(frame, rows[1], app);
        rows[2]
    } else {
        rows[1]
    };

    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
        .split(bottom);

    draw_chart(frame, columns[0], app);

    if app.has_errors() && !app.is_testing() {
        // Give error messages the whole column so they're fully readable.
        draw_summary(frame, columns[1], app);
    } else {
        let right = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(7), Constraint::Min(4)])
            .split(columns[1]);

        draw_samples(frame, right[0], app);
        draw_summary(frame, right[1], app);
    }
}

struct CardSpec<'a> {
    title: &'a str,
    value: Option<f64>,
    unit: &'static str,
    color: Rgb,
    failed: bool,
    active: bool,
    /// For ms metrics: lower is better, so the bar shows "quality" (full = great).
    quality_bar: bool,
}

fn draw_card(frame: &mut Frame, area: Rect, spec: &CardSpec, tick: usize, flash: bool) {
    let border = if flash { WHITE } else { spec.color };
    let inner_w = area.width.saturating_sub(2) as usize;

    let lines: Vec<Line> = match spec.value {
        Some(value) => {
            let text = if value >= 100.0 {
                format!("{value:.0}")
            } else {
                format!("{value:.1}")
            };

            let rows = big_text(&text);
            let mut lines: Vec<Line> = if rows[0].chars().count() <= inner_w {
                rows.into_iter()
                    .map(|row| Line::from(Span::styled(row, bold(spec.color))))
                    .collect()
            } else {
                vec![
                    Line::from(""),
                    Line::from(Span::styled(format!("{value:.2}"), bold(spec.color))),
                    Line::from(""),
                ]
            };

            lines.push(Line::from(Span::styled(spec.unit, fg(MUTED))));

            let frac = if spec.quality_bar {
                (1.0 - value / 100.0).clamp(0.05, 1.0)
            } else {
                let max = MBPS_SCALES
                    .iter()
                    .copied()
                    .find(|scale| *scale >= value)
                    .unwrap_or(10000.0);
                value / max
            };

            lines.push(Line::from(fancy_bar(
                inner_w.saturating_sub(2),
                frac,
                spec.color,
                tick,
                spec.active,
            )));

            lines
        }
        None if spec.failed => vec![
            Line::from(""),
            Line::from(Span::styled("✗ FAILED", bold(RED))),
            Line::from(Span::styled("see the issues panel", fg(MUTED))),
        ],
        None if spec.active => {
            let mut spinner = vec![
                Span::styled(SPIN[(tick / 2) % SPIN.len()], bold(spec.color)),
                Span::raw(" "),
            ];
            spinner.extend(shimmer("measuring", spec.color, spec.color, tick, false));

            vec![Line::from(""), Line::from(spinner)]
        }
        None => vec![
            Line::from(""),
            Line::from(Span::styled("- - -", fg(MUTED))),
            Line::from(Span::styled(spec.unit, fg(TRACK))),
        ],
    };

    let card = Paragraph::new(lines)
        .alignment(Alignment::Center)
        .block(rounded_block(spec.title, rgb(border)));

    frame.render_widget(card, area);
}

/// Animated "packet stream" between YOU and the SERVER.
fn draw_flow(frame: &mut Frame, area: Rect, app: &App) {
    let color = phase_color(app);
    let inner_w = area.width.saturating_sub(2) as usize;

    let left = " YOU ";
    let right = " ☁ SERVER ";
    let mid = inner_w.saturating_sub(left.chars().count() + right.chars().count());

    if mid < 8 {
        return;
    }

    let mut cells: Vec<(char, Rgb)> = vec![('─', TRACK); mid];
    let mut put = |index: usize, c: char, col: Rgb| {
        if index < mid {
            cells[index] = (c, col);
        }
    };

    let title = match app.phase {
        Phase::Download => {
            // Packets travel right → left, toward you.
            let spacing = 7;
            let offset = app.tick % mid;
            for start in (0..mid).step_by(spacing) {
                let head = (start + mid - offset) % mid;
                put(head, '●', color);
                put(head + 1, '━', dim(color, 0.6));
                put(head + 2, '╌', dim(color, 0.35));
            }
            "downstream ◀◀◀"
        }
        Phase::Upload => {
            // Packets travel left → right, toward the server.
            let spacing = 7;
            let offset = app.tick % mid;
            for start in (0..mid).step_by(spacing) {
                let head = (start + offset) % mid;
                put(head, '●', color);
                if head >= 1 {
                    put(head - 1, '━', dim(color, 0.6));
                }
                if head >= 2 {
                    put(head - 2, '╌', dim(color, 0.35));
                }
            }
            "upstream ▶▶▶"
        }
        Phase::Latency => {
            // A single ping bounces back and forth.
            let period = 2 * (mid - 1);
            let p = (app.tick * 2) % period;
            let head = if p < mid { p } else { period - p };
            put(head, '◆', color);
            if head >= 1 {
                put(head - 1, '━', dim(color, 0.5));
            }
            put(head + 1, '━', dim(color, 0.5));
            "ping ⇄"
        }
        Phase::Idle => {
            let head = (app.tick / 2) % mid;
            put(head, '•', lerp(TRACK, color, breath(app.tick, 0.2)));
            "idle"
        }
    };

    let mut spans = vec![Span::styled(left, bold(CYAN))];
    for (c, col) in cells {
        spans.push(Span::styled(c.to_string(), fg(col)));
    }
    spans.push(Span::styled(right, bold(MAGENTA)));

    let panel = Paragraph::new(Line::from(spans)).block(rounded_block(
        &format!("Packet stream · {title}"),
        rgb(dim(color, 0.8)),
    ));

    frame.render_widget(panel, area);
}

fn draw_chart(frame: &mut Frame, area: Rect, app: &App) {
    let color = phase_color(app);

    let waiting = app.live_down.is_empty() && app.live_up.is_empty();

    if waiting {
        // Alive-looking idle animation: two drifting sine waves.
        let phase = app.tick as f64 * 0.18;
        let points: Vec<(f64, f64)> = (0..160)
            .map(|i| {
                let x = i as f64 * 100.0 / 159.0;
                let y = (x * 0.22 - phase).sin() * 0.6 + (x * 0.07 + phase * 0.5).sin() * 0.3;
                (x, y)
            })
            .collect();

        let datasets = vec![
            Dataset::default()
                .marker(Marker::Braille)
                .graph_type(GraphType::Line)
                .style(fg(dim(color, 0.7)))
                .data(points.as_slice()),
        ];

        let chart = Chart::new(datasets)
            .block(rounded_block("Live throughput", rgb(dim(color, 0.8))))
            .x_axis(Axis::default().bounds([0.0, 100.0]))
            .y_axis(Axis::default().bounds([-1.2, 1.2]));

        frame.render_widget(chart, area);
        return;
    }

    let last_t = |points: &[(f64, f64)]| points.last().map_or(0.0, |p| p.0);
    let x_max = last_t(&app.live_down).max(last_t(&app.live_up)).max(6.0);

    let y_peak = app
        .live_down
        .iter()
        .chain(app.live_up.iter())
        .map(|p| p.1)
        .fold(10.0_f64, f64::max);
    let y_max = y_peak * 1.15;

    let datasets = vec![
        Dataset::default()
            .name("Download")
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(fg(GREEN))
            .data(app.live_down.as_slice()),
        Dataset::default()
            .name("Upload")
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(fg(BLUE))
            .data(app.live_up.as_slice()),
    ];

    let chart = Chart::new(datasets)
        .block(rounded_block("Live throughput", rgb(dim(color, 0.8))))
        .x_axis(
            Axis::default()
                .style(fg(MUTED))
                .bounds([0.0, x_max])
                .labels(vec![
                    Span::styled("0s", fg(MUTED)),
                    Span::styled(format!("{x_max:.0}s"), fg(MUTED)),
                ]),
        )
        .y_axis(
            Axis::default()
                .style(fg(MUTED))
                .bounds([0.0, y_max])
                .labels(vec![
                    Span::styled("0", fg(MUTED)),
                    Span::styled(format!("{y_max:.0} Mbps"), fg(MUTED)),
                ]),
        );

    frame.render_widget(chart, area);
}

fn draw_samples(frame: &mut Frame, area: Rect, app: &App) {
    let mut lines: Vec<Line> = Vec::new();

    if app.samples.is_empty() {
        let dots = ".".repeat((app.tick / 6) % 4);
        lines.push(Line::from(Span::styled(
            if app.phase == Phase::Latency {
                format!("Listening{dots}")
            } else {
                "No samples yet".to_string()
            },
            fg(MUTED),
        )));
    } else {
        let max = app
            .samples
            .iter()
            .cloned()
            .fold(f64::MIN, f64::max)
            .max(1.0);

        for (index, value) in app.samples.iter().enumerate() {
            let bar_len = ((value / max) * 16.0).round().max(1.0) as usize;
            let color = lerp(GREEN, RED, value / 120.0);

            lines.push(Line::from(vec![
                Span::styled(format!("#{} ", index + 1), fg(MUTED)),
                Span::styled("█".repeat(bar_len), fg(color)),
                Span::raw(format!(" {value:.1} ms")),
            ]));
        }
    }

    let panel = Paragraph::new(lines).block(rounded_block("Latency samples", rgb(YELLOW)));
    frame.render_widget(panel, area);
}

fn verdict(download: f64) -> (usize, &'static str) {
    if download < 3.0 {
        (1, "Barely crawling — basic browsing only")
    } else if download < 10.0 {
        (2, "Fine for browsing and SD video")
    } else if download < 35.0 {
        (3, "Smooth HD streaming & video calls")
    } else if download < 100.0 {
        (4, "Great for 4K streaming and busy homes")
    } else {
        (5, "Blazing fast — gaming, 8K, huge downloads")
    }
}

fn latency_note(latency: f64) -> &'static str {
    if latency < 20.0 {
        "Latency: excellent for gaming"
    } else if latency < 50.0 {
        "Latency: good for calls & games"
    } else if latency < 100.0 {
        "Latency: fair — some lag in games"
    } else {
        "Latency: high — expect lag"
    }
}

fn draw_summary(frame: &mut Frame, area: Rect, app: &App) {
    let mut lines: Vec<Line> = Vec::new();

    let errors = [
        ("Latency", &app.latency_err),
        ("Download", &app.download_err),
        ("Upload", &app.upload_err),
    ];

    for (name, error) in errors {
        if let Some(message) = error {
            lines.push(Line::from(Span::styled(format!("✗ {name}"), bold(RED))));
            lines.push(Line::from(Span::styled(
                format!("  {message}"),
                fg((190, 190, 200)),
            )));
        }
    }

    if app.is_testing() {
        if lines.is_empty() {
            let dots = ".".repeat((app.tick / 6) % 4);
            lines.push(Line::from(Span::styled(
                format!("Crunching the numbers{dots}"),
                fg(MUTED),
            )));
        }
    } else if app.has_results() {
        if !lines.is_empty() {
            lines.push(Line::from(""));
        }

        if let Some(download) = app.down.target {
            let (stars, message) = verdict(download);

            let mut star_line = shimmer(&"★".repeat(stars), GOLD, GOLD, app.tick, true);
            star_line.push(Span::styled("☆".repeat(5 - stars), fg(TRACK)));
            lines.push(Line::from(star_line));
            lines.push(Line::from(Span::styled(message, fg(WHITE))));
        }

        if let Some(latency) = app.lat.target {
            lines.push(Line::from(Span::styled(latency_note(latency), fg(MUTED))));
        }
    } else if lines.is_empty() {
        let tone = lerp(MUTED, CYAN, breath(app.tick, 0.12));
        lines.push(Line::from(Span::styled(
            "Press R to launch a test",
            fg(tone),
        )));
    }

    let (title, color) = if app.has_errors() {
        ("Issues & verdict", RED)
    } else if app.has_results() && !app.is_testing() {
        ("Verdict", GOLD)
    } else {
        ("Summary", MUTED)
    };

    let panel = Paragraph::new(lines)
        .wrap(Wrap { trim: true })
        .block(rounded_block(title, rgb(color)));

    frame.render_widget(panel, area);
}

fn draw_history(frame: &mut Frame, area: Rect, app: &App) {
    let header = Row::new(["Date / time", "Download", "", "Upload", "Latency", "Jitter"])
        .style(bold(CYAN))
        .bottom_margin(1);

    let max_download = app
        .history
        .iter()
        .filter_map(|record| record.download)
        .fold(1.0_f64, f64::max);

    let rows: Vec<Row> = app
        .history
        .iter()
        .map(|record| {
            let bar = match record.download {
                Some(value) => Line::from(fancy_bar(12, value / max_download, GREEN, 0, false)),
                None => Line::from(Span::styled("failed", fg(RED))),
            };

            Row::new(vec![
                Cell::from(record.timestamp.clone()).style(fg((170, 175, 190))),
                Cell::from(metric(record.download, "Mbps")).style(fg(GREEN)),
                Cell::from(bar),
                Cell::from(metric(record.upload, "Mbps")).style(fg(BLUE)),
                Cell::from(metric(record.latency, "ms")).style(fg(YELLOW)),
                Cell::from(metric(record.jitter, "ms")).style(fg(MAGENTA)),
            ])
        })
        .collect();

    let widths = [
        Constraint::Length(21),
        Constraint::Length(14),
        Constraint::Length(14),
        Constraint::Length(14),
        Constraint::Length(12),
        Constraint::Min(10),
    ];

    let title = if app.history.is_empty() {
        "Recent tests — none saved yet, press R"
    } else {
        "Recent tests (newest first)"
    };

    let table = Table::new(rows, widths)
        .header(header)
        .block(rounded_block(title, rgb(CYAN)));

    frame.render_widget(table, area);
}

fn draw_keys(frame: &mut Frame, area: Rect, app: &App) {
    let key = Style::default()
        .fg(Color::Black)
        .bg(rgb(CYAN))
        .add_modifier(Modifier::BOLD);
    let label = fg((170, 175, 190));

    let toggle = match app.view {
        View::Dashboard => " History ",
        View::History => " Dashboard ",
    };

    let run_label = if app.is_testing() {
        " Test running "
    } else {
        " Run test "
    };

    let line = Line::from(vec![
        Span::styled(" R ", key),
        Span::styled(run_label, label),
        Span::styled(" H ", key),
        Span::styled(toggle, label),
        Span::styled(" Q ", key),
        Span::styled(" Quit ", label),
    ]);

    frame.render_widget(Paragraph::new(line), area);
}

// ───────────────────────────── Terminal handling ─────────────────────────────

/// Restores the terminal even if the program exits early with an error or panic.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Open and initialize the database before entering terminal mode.
    let connection = init_database()?;
    let client = build_client()?;

    enable_raw_mode()?;
    let _guard = TerminalGuard;

    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;

    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    terminal.clear()?;

    let (tx, rx): (Sender<TestUpdate>, Receiver<TestUpdate>) = mpsc::channel();

    let mut app = App::new(load_history(&connection).unwrap_or_default());
    let mut running = true;

    while running {
        while let Ok(update) = rx.try_recv() {
            apply_update(&mut app, update, &connection);
        }

        app.update_frame();
        terminal.draw(|frame| ui(frame, &app))?;

        // ~20 fps: smooth animation without burning CPU.
        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                // Ignore key-release / repeat events (they fire on some platforms).
                if key.kind != KeyEventKind::Press {
                    continue;
                }

                match key.code {
                    KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => running = false,

                    KeyCode::Char('h') | KeyCode::Char('H') => {
                        app.view = match app.view {
                            View::Dashboard => {
                                app.history = load_history(&connection).unwrap_or_default();
                                View::History
                            }
                            View::History => View::Dashboard,
                        };
                    }

                    KeyCode::Char('r') | KeyCode::Char('R') if !app.is_testing() => {
                        app.reset_for_test();
                        start_test(client.clone(), tx.clone());
                    }

                    _ => {}
                }
            }
        }
    }

    terminal.show_cursor()?;

    Ok(())
}

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
    widgets::{Axis, Block, Borders, Cell, Chart, Dataset, GraphType, Paragraph, Row, Table, Wrap},
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

const ACCENT: Rgb = (110, 200, 240);
const DOWN: Rgb = (90, 210, 170);
const UP: Rgb = (160, 140, 250);
const LAT: Rgb = (235, 195, 110);
const JIT: Rgb = (150, 160, 190);
const BAD: Rgb = (240, 110, 110);
const MUTED: Rgb = (125, 131, 150);
const TRACK: Rgb = (62, 66, 80);

fn rgb(c: Rgb) -> Color {
    Color::Rgb(c.0, c.1, c.2)
}

fn fg(c: Rgb) -> Style {
    Style::default().fg(rgb(c))
}

fn bold(c: Rgb) -> Style {
    fg(c).add_modifier(Modifier::BOLD)
}

fn muted() -> Style {
    fg(MUTED)
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

    fn elapsed_secs(&self) -> u64 {
        if self.is_testing() {
            self.test_started.map_or(0, |t| t.elapsed().as_secs())
        } else {
            self.elapsed_final
        }
    }
}

// ───────────────────────────── Database ─────────────────────────────

fn db_path() -> std::path::PathBuf {
    let mut dir = dirs::data_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    dir.push("netpulse");
    let _ = std::fs::create_dir_all(&dir);
    dir.push("netpulse.db");
    dir
}

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

// ───────────────────────────── UI ─────────────────────────────

const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn metric(value: Option<f64>, unit: &str) -> String {
    match value {
        Some(value) => format!("{value:.2} {unit}"),
        None => "—".into(),
    }
}

fn phase_color(app: &App) -> Rgb {
    match app.phase {
        Phase::Latency => LAT,
        Phase::Download => DOWN,
        Phase::Upload => UP,
        Phase::Idle => {
            if !app.ran {
                ACCENT
            } else if app.has_errors() {
                LAT
            } else {
                DOWN
            }
        }
    }
}

fn ui(frame: &mut Frame, app: &App) {
    let info_height = if app.has_errors() { 5 } else { 3 };

    let areas = Layout::default()
        .direction(Direction::Vertical)
        .horizontal_margin(2)
        .vertical_margin(1)
        .constraints([
            Constraint::Length(1), // header
            Constraint::Length(1), // spacer
            Constraint::Length(1), // status
            Constraint::Length(1), // progress line
            Constraint::Length(1), // spacer
            Constraint::Min(6),    // main
            Constraint::Length(1), // keys
        ])
        .split(frame.area());

    draw_header(frame, areas[0], app);
    draw_status(frame, areas[2], app);
    draw_progress(frame, areas[3], app);

    match app.view {
        View::Dashboard => draw_dashboard(frame, areas[5], app, info_height),
        View::History => draw_history(frame, areas[5], app),
    }

    draw_keys(frame, areas[6], app);
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let left = Line::from(vec![
        Span::styled("netpulse", bold(ACCENT)),
        Span::styled("  internet speed", muted()),
    ]);

    let tab = |label: &'static str, active: bool| {
        if active {
            Span::styled(label, bold(ACCENT).add_modifier(Modifier::UNDERLINED))
        } else {
            Span::styled(label, muted())
        }
    };

    let right = Line::from(vec![
        tab("dashboard", app.view == View::Dashboard),
        Span::raw("   "),
        tab("history", app.view == View::History),
    ]);

    frame.render_widget(Paragraph::new(left), area);
    frame.render_widget(Paragraph::new(right).alignment(Alignment::Right), area);
}

fn draw_status(frame: &mut Frame, area: Rect, app: &App) {
    let color = phase_color(app);

    let line = if app.is_testing() {
        let label = match app.phase {
            Phase::Latency => "Measuring latency",
            Phase::Download => "Measuring download",
            _ => "Measuring upload",
        };

        Line::from(vec![
            Span::styled(SPIN[(app.tick / 2) % SPIN.len()], fg(color)),
            Span::raw(" "),
            Span::raw(label),
            Span::styled(format!("  ·  {}s", app.elapsed_secs()), muted()),
        ])
    } else if app.ran {
        let icon = if app.has_errors() { "!" } else { "✓" };

        Line::from(vec![
            Span::styled(icon, bold(color)),
            Span::raw(" "),
            Span::raw(app.status.clone()),
            Span::styled(format!("  ·  {}s", app.elapsed_secs()), muted()),
        ])
    } else {
        Line::from(Span::styled("Ready — press r to start a test", muted()))
    };

    frame.render_widget(Paragraph::new(line), area);
}

/// A thin single-line progress indicator.
fn draw_progress(frame: &mut Frame, area: Rect, app: &App) {
    let width = area.width as usize;
    let filled = ((app.shown_progress / 100.0) * width as f64).round() as usize;
    let filled = filled.min(width);

    let color = phase_color(app);

    let line = Line::from(vec![
        Span::styled("━".repeat(filled), fg(color)),
        Span::styled("─".repeat(width - filled), fg(TRACK)),
    ]);

    frame.render_widget(Paragraph::new(line), area);
}

fn draw_dashboard(frame: &mut Frame, area: Rect, app: &App, info_height: u16) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),           // metrics
            Constraint::Length(1),           // spacer
            Constraint::Min(5),              // chart
            Constraint::Length(info_height), // summary / errors
        ])
        .split(area);

    draw_metrics(frame, rows[0], app);
    draw_chart(frame, rows[2], app);
    draw_info(frame, rows[3], app);
}

fn draw_metrics(frame: &mut Frame, area: Rect, app: &App) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(25); 4])
        .split(area);

    // (label, dot color, value, unit, failed, measuring)
    let metrics: [(&str, Rgb, Option<f64>, &str, bool, bool); 4] = [
        (
            "Download",
            DOWN,
            app.down.value(),
            "Mbps",
            app.download_err.is_some(),
            app.phase == Phase::Download,
        ),
        (
            "Upload",
            UP,
            app.up.value(),
            "Mbps",
            app.upload_err.is_some(),
            app.phase == Phase::Upload,
        ),
        (
            "Latency",
            LAT,
            app.lat.value(),
            "ms",
            app.latency_err.is_some(),
            app.phase == Phase::Latency,
        ),
        (
            "Jitter",
            JIT,
            app.jit.value(),
            "ms",
            false,
            app.phase == Phase::Latency,
        ),
    ];

    for (column, (label, color, value, unit, failed, measuring)) in columns.iter().zip(metrics) {
        let value_line = match value {
            Some(v) => {
                let text = if v >= 100.0 {
                    format!("{v:.0}")
                } else {
                    format!("{v:.1}")
                };

                Line::from(vec![
                    Span::styled(text, Style::default().add_modifier(Modifier::BOLD)),
                    Span::styled(format!(" {unit}"), muted()),
                ])
            }
            None if failed => Line::from(Span::styled("failed", fg(BAD))),
            None if measuring => Line::from(Span::styled("…", muted())),
            None => Line::from(Span::styled("—", muted())),
        };

        let lines = vec![
            Line::from(vec![
                Span::styled("● ", fg(color)),
                Span::styled(label, muted()),
            ]),
            value_line,
        ];

        frame.render_widget(Paragraph::new(lines), *column);
    }
}

fn draw_chart(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(fg(TRACK))
        .title(Span::styled(" throughput ", muted()));

    if app.live_down.is_empty() && app.live_up.is_empty() {
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let text = if app.is_testing() {
            "Warming up…"
        } else {
            "Press r to run a test"
        };

        if inner.height > 0 {
            let middle = Rect {
                x: inner.x,
                y: inner.y + inner.height / 2,
                width: inner.width,
                height: 1,
            };

            frame.render_widget(
                Paragraph::new(Span::styled(text, muted())).alignment(Alignment::Center),
                middle,
            );
        }
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
            .name("download")
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(fg(DOWN))
            .data(app.live_down.as_slice()),
        Dataset::default()
            .name("upload")
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(fg(UP))
            .data(app.live_up.as_slice()),
    ];

    let chart = Chart::new(datasets)
        .block(block)
        .x_axis(
            Axis::default()
                .style(fg(TRACK))
                .bounds([0.0, x_max])
                .labels(vec![
                    Span::styled("0s", muted()),
                    Span::styled(format!("{x_max:.0}s"), muted()),
                ]),
        )
        .y_axis(
            Axis::default()
                .style(fg(TRACK))
                .bounds([0.0, y_max])
                .labels(vec![
                    Span::styled("0", muted()),
                    Span::styled(format!("{y_max:.0} Mbps"), muted()),
                ]),
        );

    frame.render_widget(chart, area);
}

fn verdict(download: f64) -> &'static str {
    if download < 3.0 {
        "Basic browsing only"
    } else if download < 10.0 {
        "Fine for browsing and SD video"
    } else if download < 35.0 {
        "Smooth HD streaming and video calls"
    } else if download < 100.0 {
        "Great for 4K streaming and busy homes"
    } else {
        "Very fast — gaming, 8K, large downloads"
    }
}

fn latency_note(latency: f64) -> &'static str {
    if latency < 20.0 {
        "excellent latency for gaming"
    } else if latency < 50.0 {
        "good latency for calls and games"
    } else if latency < 100.0 {
        "fair latency, some lag in games"
    } else {
        "high latency, expect lag"
    }
}

fn draw_info(frame: &mut Frame, area: Rect, app: &App) {
    let mut lines: Vec<Line> = Vec::new();

    let errors = [
        ("Latency", &app.latency_err),
        ("Download", &app.download_err),
        ("Upload", &app.upload_err),
    ];

    for (name, error) in errors {
        if let Some(message) = error {
            lines.push(Line::from(vec![
                Span::styled(format!("✗ {name}  "), bold(BAD)),
                Span::styled(message.clone(), muted()),
            ]));
        }
    }

    if !app.samples.is_empty() {
        let values: Vec<String> = app.samples.iter().map(|v| format!("{v:.1}")).collect();

        lines.push(Line::from(vec![
            Span::styled("Latency samples  ", muted()),
            Span::raw(values.join(" · ")),
            Span::styled(" ms", muted()),
        ]));
    }

    if !app.is_testing() && app.has_results() {
        let mut spans = vec![Span::styled("Verdict  ", muted())];

        if let Some(download) = app.down.target {
            spans.push(Span::raw(verdict(download)));
        }

        if let Some(latency) = app.lat.target {
            if app.down.target.is_some() {
                spans.push(Span::styled(" · ", muted()));
            }
            spans.push(Span::raw(latency_note(latency)));
        }

        lines.push(Line::from(spans));
    }

    let panel = Paragraph::new(lines).wrap(Wrap { trim: true });
    frame.render_widget(panel, area);
}

fn draw_history(frame: &mut Frame, area: Rect, app: &App) {
    let rows_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(3)])
        .split(area);

    frame.render_widget(
        Paragraph::new(Span::styled("Recent tests, newest first", muted())),
        rows_layout[0],
    );

    if app.history.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "No saved tests yet — press r to run one",
                muted(),
            )),
            rows_layout[1],
        );
        return;
    }

    let header = Row::new(["Date / time", "Download", "Upload", "Latency", "Jitter"])
        .style(muted())
        .bottom_margin(1);

    let rows: Vec<Row> = app
        .history
        .iter()
        .map(|record| {
            Row::new(vec![
                Cell::from(record.timestamp.clone()).style(muted()),
                Cell::from(metric(record.download, "Mbps")).style(fg(DOWN)),
                Cell::from(metric(record.upload, "Mbps")).style(fg(UP)),
                Cell::from(metric(record.latency, "ms")).style(fg(LAT)),
                Cell::from(metric(record.jitter, "ms")).style(fg(JIT)),
            ])
        })
        .collect();

    let widths = [
        Constraint::Length(21),
        Constraint::Length(15),
        Constraint::Length(15),
        Constraint::Length(12),
        Constraint::Min(10),
    ];

    frame.render_widget(Table::new(rows, widths).header(header), rows_layout[1]);
}

fn draw_keys(frame: &mut Frame, area: Rect, app: &App) {
    let key = |k: &'static str| Span::styled(k, bold(ACCENT));

    let run = if app.is_testing() { " running" } else { " run" };
    let toggle = match app.view {
        View::Dashboard => " history",
        View::History => " dashboard",
    };

    let line = Line::from(vec![
        key("r"),
        Span::styled(run, muted()),
        Span::raw("    "),
        key("h"),
        Span::styled(toggle, muted()),
        Span::raw("    "),
        key("q"),
        Span::styled(" quit", muted()),
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

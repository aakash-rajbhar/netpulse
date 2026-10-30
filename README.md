# NetPulse

A terminal-based internet speed test written in Rust. NetPulse measures download speed, upload speed, latency, and jitter, visualizes a live test in the terminal, and keeps a local history of past results in SQLite.

## Features

- Live download, upload, latency, and jitter measurements.
- Animated dashboard with metric cards, a packet-flow view, and a live throughput chart.
- Latency samples and a plain-language verdict after each test.
- Tests run on a background thread, so the UI stays responsive.
- Results are saved to a local SQLite database.
- Recent history view with per-record download bars.

## Tech Stack

| Technology | Purpose |
|---|---|
| Rust | Application logic and measurements |
| Ratatui | Terminal UI and charts |
| Crossterm | Terminal control and key input |
| Reqwest | HTTP requests for speed tests |
| rusqlite | SQLite persistence |

## Requirements

- A recent stable Rust toolchain (the project uses the 2024 edition).
- An active internet connection (the speed tests use public Cloudflare endpoints).

## Getting Started

```bash
git clone https://github.com/aakash-rajbhar/netpulse.git
cd netpulse
cargo run --release
```

To build without running:

```bash
cargo build --release
```

The binary is written to `target/release/netpulse`.

## Usage

| Key | Action |
|---|---|
| `R` | Start a new speed test (ignored while a test is running) |
| `H` | Toggle between the dashboard and history views |
| `Q` or `Esc` | Quit |

A test runs in three phases:

1. **Latency** — five HTTP requests, 250 ms apart.
2. **Download** — repeated 10 MB requests for up to 8 seconds.
3. **Upload** — 512 KiB chunks for up to 8 seconds (128 MiB cap).

When the test finishes, the result is saved even if one or more metrics failed.

## How Measurements Work

### Latency

Each sample is the time to complete a GET request to `https://www.google.com/generate_204`. The reported latency is the mean of all successful samples.

### Jitter

Jitter is the average absolute difference between consecutive successful latency samples. For samples `L1 … Ln`:

```text
jitter = Σ |L(i+1) − L(i)| / (n − 1)
```

Jitter requires at least two successful samples.

### Download

NetPulse repeatedly requests 10 MB from `https://speed.cloudflare.com/__down?bytes=10000000` and streams the response, stopping when the 8-second deadline is reached. Throughput is:

```text
Mbps = total_bytes × 8 / elapsed_seconds / 1,000,000
```

Live samples are emitted roughly every 100 ms and smoothed for display.

### Upload

NetPulse posts 512 KiB chunks to `https://speed.cloudflare.com/__up` until the 8-second deadline or a 128 MiB cap is reached. A partial upload still produces a result. If the first attempt fails, it retries once over HTTP/1.1, since some networks and proxies stall HTTP/2 uploads.

## Data Storage

NetPulse creates `netpulse.db` in the working directory on first run. The `test_history` table stores:

| Column | Type |
|---|---|
| `id` | INTEGER PRIMARY KEY |
| `tested_at` | TEXT (local time) |
| `download_mbps` | REAL, nullable |
| `upload_mbps` | REAL, nullable |
| `latency_ms` | REAL, nullable |
| `jitter_ms` | REAL, nullable |

Failed metrics are stored as `NULL`. The history view shows the latest 10 records; older records are retained.

## Project Structure

```text
netpulse/
├── Cargo.toml
├── Cargo.lock
├── src/
│   └── main.rs
└── netpulse.db   # created on first run
```

## Limitations

- Latency uses HTTP response time, not ICMP ping.
- Download and upload figures are application-level estimates, not a connection's theoretical maximum.
- Results vary with network congestion, server load, and connection conditions.
- Tests depend on Cloudflare and Google endpoints being reachable.

## License

MIT

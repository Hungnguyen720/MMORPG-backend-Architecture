//! Tick-time and bandwidth metrics, served in Prometheus text format on a
//! plain TCP port (`GET /metrics`), and as JSON on `GET /metrics.json` for
//! the load-test bots.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone, Default)]
pub struct ZoneMetrics {
    pub zone: String,
    pub layer: u16,
    pub port: u16,
    pub players: usize,
    pub entities: usize,
    /// 20 Hz world tick (AI + snapshots + send), last 10 s, milliseconds.
    pub tick_p50: f64,
    pub tick_p99: f64,
    pub tick_max: f64,
    pub ticks: usize,
    /// Mean and max bytes per second sent to one client (renet payload).
    pub bytes_out_avg: f64,
    pub bytes_out_max: f64,
    /// Mean and max bytes per second received from one client.
    pub bytes_in_avg: f64,
    /// Entities per snapshot, mean.
    pub view_avg: f64,
    pub queue_casts: u32,
    pub queue_gap_max_ms: f64,
    pub movement_corrections: u64,
    /// Network thread: one loop (recv, decrypt, encrypt, send), ms.
    pub net_loop_p99: f64,
    pub net_loop_max: f64,
    pub send_errors: u64,
}

pub type Shared = Arc<Mutex<Vec<ZoneMetrics>>>;

/// Rolling window of tick durations in microseconds.
#[derive(Default)]
pub struct TickWindow {
    samples: Vec<u32>,
}

impl TickWindow {
    pub fn push(&mut self, us: u32) {
        self.samples.push(us);
    }

    /// (p50, p99, max) in ms, then clears the window.
    pub fn take(&mut self) -> (f64, f64, f64, usize) {
        if self.samples.is_empty() {
            return (0.0, 0.0, 0.0, 0);
        }
        self.samples.sort_unstable();
        let n = self.samples.len();
        let q = |p: f64| self.samples[((n - 1) as f64 * p).round() as usize] as f64 / 1000.0;
        let r = (q(0.5), q(0.99), self.samples[n - 1] as f64 / 1000.0, n);
        self.samples.clear();
        r
    }
}

pub fn render_prometheus(all: &[ZoneMetrics]) -> String {
    let mut s = String::new();
    let mut metric = |name: &str, help: &str, get: &dyn Fn(&ZoneMetrics) -> f64| {
        s.push_str(&format!("# HELP {name} {help}\n# TYPE {name} gauge\n"));
        for m in all {
            s.push_str(&format!(
                "{name}{{zone=\"{}\",layer=\"{}\"}} {}\n",
                m.zone,
                m.layer,
                get(m)
            ));
        }
    };
    metric("zone_players", "Connected players", &|m| m.players as f64);
    metric("zone_entities", "Live entities", &|m| m.entities as f64);
    metric("zone_tick_p50_ms", "World tick p50 over 10 s", &|m| m.tick_p50);
    metric("zone_tick_p99_ms", "World tick p99 over 10 s", &|m| m.tick_p99);
    metric("zone_tick_max_ms", "World tick max over 10 s", &|m| m.tick_max);
    metric("zone_client_bytes_out_avg", "Mean bytes/s sent per client", &|m| m.bytes_out_avg);
    metric("zone_client_bytes_out_max", "Max bytes/s sent to one client", &|m| m.bytes_out_max);
    metric("zone_client_bytes_in_avg", "Mean bytes/s received per client", &|m| m.bytes_in_avg);
    metric("zone_view_entities_avg", "Mean entities per snapshot", &|m| m.view_avg);
    metric("zone_queue_gap_max_ms", "Worst spell-queue gap, 10 s", &|m| m.queue_gap_max_ms);
    metric("zone_movement_corrections_total", "Rejected moves", &|m| m.movement_corrections as f64);
    metric("zone_net_loop_p99_ms", "Network thread loop p99", &|m| m.net_loop_p99);
    metric("zone_send_errors_total", "UDP packets the kernel refused", &|m| m.send_errors as f64);
    s
}

pub fn render_json(all: &[ZoneMetrics]) -> String {
    let rows: Vec<String> = all
        .iter()
        .map(|m| {
            format!(
                "{{\"zone\":\"{}\",\"layer\":{},\"port\":{},\"players\":{},\"entities\":{},\"tick_p50_ms\":{:.3},\"tick_p99_ms\":{:.3},\"tick_max_ms\":{:.3},\"ticks\":{},\"bytes_out_avg\":{:.0},\"bytes_out_max\":{:.0},\"bytes_in_avg\":{:.0},\"view_avg\":{:.1},\"queue_casts\":{},\"queue_gap_max_ms\":{:.3},\"movement_corrections\":{},\"net_loop_p99_ms\":{:.3},\"net_loop_max_ms\":{:.3},\"send_errors\":{}}}",
                m.zone,
                m.layer,
                m.port,
                m.players,
                m.entities,
                m.tick_p50,
                m.tick_p99,
                m.tick_max,
                m.ticks,
                m.bytes_out_avg,
                m.bytes_out_max,
                m.bytes_in_avg,
                m.view_avg,
                m.queue_casts,
                m.queue_gap_max_ms,
                m.movement_corrections,
                m.net_loop_p99,
                m.net_loop_max,
                m.send_errors
            )
        })
        .collect();
    format!("[{}]", rows.join(","))
}

/// Serve metrics forever on `port`. Tiny on purpose: one request per
/// connection, no keep-alive.
pub fn serve(port: u16, shared: Shared) {
    let listener = match TcpListener::bind(("0.0.0.0", port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("metrics: cannot bind TCP {port}: {e}");
            return;
        }
    };
    println!("metrics on http://0.0.0.0:{port}/metrics");
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
            let mut buf = [0u8; 1024];
            let n = stream.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let all = shared.lock().unwrap().clone();
            let (ctype, body) = if req.starts_with("GET /metrics.json") {
                ("application/json", render_json(&all))
            } else {
                ("text/plain; version=0.0.4", render_prometheus(&all))
            };
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
}

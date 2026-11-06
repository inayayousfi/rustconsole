//! UDP relay that imitates a bad network without root access. Host-to-player traffic
//! passes a rate limit with a bounded router-style buffer; both directions get delay,
//! jitter, and random loss.

use std::collections::VecDeque;
use std::fs;
use std::net::{SocketAddr, UdpSocket};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const MAX_DATAGRAM: usize = 2_048;
const BUFFER_TIME: Duration = Duration::from_millis(50);
const MAX_QUEUED_PACKETS: usize = 16_384;
const ARRIVAL_LOG_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone, Copy)]
struct Step {
    at: Duration,
    rate_bits_per_second: u64,
    delay: Duration,
    jitter: Duration,
    loss_ppm: u32,
}

fn parse_scenario(path: &Path) -> Result<Vec<Step>, Box<dyn std::error::Error>> {
    let mut steps = Vec::new();
    for line in fs::read_to_string(path)?.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let fields = line
            .split_whitespace()
            .map(str::parse::<f64>)
            .collect::<Result<Vec<_>, _>>()?;
        let [at, rate, delay, jitter, loss] = fields[..] else {
            return Err(format!("scenario line needs 5 numbers: {line}").into());
        };
        steps.push(Step {
            at: Duration::from_secs_f64(at),
            rate_bits_per_second: (rate * 1_000_000.0) as u64,
            delay: Duration::from_secs_f64(delay / 1_000.0),
            jitter: Duration::from_secs_f64(jitter / 1_000.0),
            loss_ppm: (loss * 10_000.0) as u32,
        });
    }
    if steps.is_empty() {
        return Err("scenario has no steps".into());
    }
    Ok(steps)
}

fn current(steps: &[Step], elapsed: Duration) -> Step {
    steps
        .iter()
        .rev()
        .find(|step| step.at <= elapsed)
        .copied()
        .unwrap_or(steps[0])
}

/// Small fast generator; statistical quality is irrelevant for simulated loss and jitter.
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

struct Packet {
    release: Instant,
    bytes: Vec<u8>,
}

#[derive(Default)]
struct Queue {
    packets: Mutex<VecDeque<Packet>>,
    ready: Condvar,
}

fn sender(queue: Arc<Queue>, send: impl Fn(&[u8])) {
    loop {
        let packet = {
            let mut packets = queue.packets.lock().unwrap();
            while packets.is_empty() {
                packets = queue.ready.wait(packets).unwrap();
            }
            packets.pop_front().unwrap()
        };
        loop {
            let now = Instant::now();
            if now >= packet.release {
                break;
            }
            let wait = packet.release - now;
            if wait > Duration::from_micros(300) {
                std::thread::sleep(wait - Duration::from_micros(200));
            } else {
                std::hint::spin_loop();
            }
        }
        send(&packet.bytes);
    }
}

struct Direction {
    started: Instant,
    steps: Arc<Vec<Step>>,
    queue: Arc<Queue>,
    random: Random,
    next_free: Instant,
    limited: bool,
}

impl Direction {
    fn new(
        started: Instant,
        steps: Arc<Vec<Step>>,
        queue: Arc<Queue>,
        limited: bool,
        seed: u64,
    ) -> Self {
        Self {
            started,
            steps,
            queue,
            random: Random(seed),
            next_free: started,
            limited,
        }
    }

    fn forward(&mut self, bytes: &[u8]) {
        let now = Instant::now();
        let step = current(&self.steps, now.saturating_duration_since(self.started));
        if (self.random.next() % 1_000_000) < u64::from(step.loss_ppm) {
            return;
        }
        let mut leaves_link = now;
        if self.limited && step.rate_bits_per_second != 0 {
            let start = self.next_free.max(now);
            if start.saturating_duration_since(now) > BUFFER_TIME {
                return;
            }
            leaves_link = start
                + Duration::from_nanos(
                    bytes.len() as u64 * 8 * 1_000_000_000 / step.rate_bits_per_second,
                );
            self.next_free = leaves_link;
        }
        let jitter = if step.jitter.is_zero() {
            Duration::ZERO
        } else {
            Duration::from_nanos(self.random.next() % step.jitter.as_nanos() as u64)
        };
        let mut packets = self.queue.packets.lock().unwrap();
        if packets.len() >= MAX_QUEUED_PACKETS {
            return;
        }
        // Keeps order: a real path with jitter here delays, it does not reorder.
        let release = (leaves_link + step.delay + jitter)
            .max(packets.back().map_or(now, |packet| packet.release));
        packets.push_back(Packet {
            release,
            bytes: bytes.to_vec(),
        });
        self.queue.ready.notify_one();
    }
}

pub fn run(
    listen: SocketAddr,
    target: SocketAddr,
    scenario: &Path,
    seconds: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let steps = Arc::new(parse_scenario(scenario)?);
    let player_side = Arc::new(UdpSocket::bind(listen)?);
    let host_side = Arc::new(UdpSocket::bind(if target.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    })?);
    host_side.connect(target)?;
    let player: Arc<Mutex<Option<SocketAddr>>> = Arc::default();
    let started = Instant::now();
    let to_host = Arc::new(Queue::default());
    let to_player = Arc::new(Queue::default());

    let socket = Arc::clone(&host_side);
    let queue = Arc::clone(&to_host);
    std::thread::spawn(move || {
        sender(queue, |bytes| {
            let _ = socket.send(bytes);
        });
    });
    let socket = Arc::clone(&player_side);
    let queue = Arc::clone(&to_player);
    let destination = Arc::clone(&player);
    std::thread::spawn(move || {
        sender(queue, |bytes| {
            if let Some(address) = *destination.lock().unwrap() {
                let _ = socket.send_to(bytes, address);
            }
        });
    });

    let socket = Arc::clone(&host_side);
    let mut direction = Direction::new(
        started,
        Arc::clone(&steps),
        Arc::clone(&to_player),
        true,
        0x9E37_79B9_7F4A_7C15,
    );
    std::thread::spawn(move || {
        let mut buffer = [0; MAX_DATAGRAM];
        let mut window_started = Instant::now();
        let mut window_bytes = 0_usize;
        while let Ok(length) = socket.recv(&mut buffer) {
            window_bytes += length;
            if window_started.elapsed() >= ARRIVAL_LOG_INTERVAL {
                eprintln!(
                    "{:.1} arrival_megabits={:.1}",
                    started.elapsed().as_secs_f64(),
                    window_bytes as f64 * 8.0 / window_started.elapsed().as_secs_f64() / 1e6
                );
                window_started = Instant::now();
                window_bytes = 0;
            }
            direction.forward(&buffer[..length]);
        }
    });
    let socket = Arc::clone(&player_side);
    let mut direction = Direction::new(started, steps, to_host, false, 0xD1B5_4A32_D192_ED03);
    std::thread::spawn(move || {
        let mut buffer = [0; MAX_DATAGRAM];
        while let Ok((length, from)) = socket.recv_from(&mut buffer) {
            *player.lock().unwrap() = Some(from);
            direction.forward(&buffer[..length]);
        }
    });
    std::thread::sleep(Duration::from_secs(seconds));
    Ok(())
}

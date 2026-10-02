//! Bounded, low-traffic startup round trips and clock alignment.

use crate::quic::{read_envelope, write_envelope};
use crate::statistics::ClockOffsetEstimate;
use rustconsole_protocol::wire::{ClockPing, ClockPong, Envelope, LatencyProbeReport, envelope};
use std::time::{Duration, Instant};

const SAMPLES: u64 = 8;
const SPACING: Duration = Duration::from_millis(25);
const TIMEOUT: Duration = Duration::from_secs(15);

pub fn elapsed_micros(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX)
}

pub fn clock_offset(pong: ClockPong, received: u64) -> Option<ClockOffsetEstimate> {
    let round_trip = received.checked_sub(pong.player_sent_at_micros)?;
    let processing = pong
        .host_sent_at_micros
        .checked_sub(pong.host_received_at_micros)?;
    let network = round_trip.checked_sub(processing)?;
    let offset = ((i128::from(pong.host_received_at_micros)
        - i128::from(pong.player_sent_at_micros))
        + (i128::from(pong.host_sent_at_micros) - i128::from(received)))
        / 2;
    Some(ClockOffsetEstimate {
        offset_micros: i64::try_from(offset).ok()?,
        uncertainty_micros: network.div_ceil(2),
    })
}

pub async fn player(
    send: &mut quinn::SendStream,
    receive: &mut quinn::RecvStream,
    started: Instant,
) -> Result<(u64, ClockOffsetEstimate), Box<dyn std::error::Error + Send + Sync>> {
    tokio::time::timeout(TIMEOUT, async {
        let mut delays = Vec::with_capacity(SAMPLES as usize);
        let mut best = None::<ClockOffsetEstimate>;
        for sequence in 1..=SAMPLES {
            let sent = elapsed_micros(started);
            write_envelope(
                send,
                Envelope {
                    body: Some(envelope::Body::ClockPing(ClockPing {
                        sequence,
                        player_sent_at_micros: sent,
                    })),
                },
            )
            .await?;
            let message = read_envelope(receive).await?;
            let received = elapsed_micros(started);
            let Some(envelope::Body::ClockPong(pong)) = message.body else {
                return Err("expected startup clock reply".into());
            };
            if pong.sequence != sequence || pong.player_sent_at_micros != sent {
                return Err("startup clock reply does not match its request".into());
            }
            let estimate =
                clock_offset(pong, received).ok_or("invalid startup clock timestamps")?;
            delays.push(received.saturating_sub(sent).max(1));
            if best.is_none_or(|current| estimate.uncertainty_micros < current.uncertainty_micros) {
                best = Some(estimate);
            }
            tokio::time::sleep(SPACING).await;
        }
        delays.sort_unstable();
        // The second-lowest of eight samples avoids trusting one exceptional minimum.
        let baseline = delays[1];
        write_envelope(
            send,
            Envelope {
                body: Some(envelope::Body::LatencyProbeReport(LatencyProbeReport {
                    baseline_round_trip_micros: baseline,
                })),
            },
        )
        .await?;
        Ok((baseline, best.ok_or("startup clock measurement missing")?))
    })
    .await
    .map_err(|_| "startup latency probe timed out")?
}

pub async fn host(
    send: &mut quinn::SendStream,
    receive: &mut quinn::RecvStream,
    mut clock: impl FnMut() -> Result<u64, Box<dyn std::error::Error + Send + Sync>>,
) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
    tokio::time::timeout(TIMEOUT, async {
        for sequence in 1..=SAMPLES {
            let message = read_envelope(receive).await?;
            let received = clock()?;
            let Some(envelope::Body::ClockPing(ping)) = message.body else {
                return Err("expected startup clock request".into());
            };
            if ping.sequence != sequence {
                return Err("invalid startup clock sequence".into());
            }
            write_envelope(
                send,
                Envelope {
                    body: Some(envelope::Body::ClockPong(ClockPong {
                        sequence,
                        player_sent_at_micros: ping.player_sent_at_micros,
                        host_received_at_micros: received,
                        host_sent_at_micros: clock()?,
                    })),
                },
            )
            .await?;
        }
        let message = read_envelope(receive).await?;
        let Some(envelope::Body::LatencyProbeReport(report)) = message.body else {
            return Err("expected startup latency report".into());
        };
        if report.baseline_round_trip_micros == 0
            || report.baseline_round_trip_micros
                > rustconsole_protocol::latency::MAX_MEASUREMENT_MICROS
        {
            return Err("invalid startup latency baseline".into());
        }
        Ok(report.baseline_round_trip_micros)
    })
    .await
    .map_err(|_| "startup latency probe timed out")?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_alignment_excludes_host_processing_and_rejects_impossible_timestamps() {
        let pong = ClockPong {
            sequence: 1,
            player_sent_at_micros: 100,
            host_received_at_micros: 1_200,
            host_sent_at_micros: 1_250,
        };
        let offset = clock_offset(pong, 350).unwrap();
        assert_eq!(offset.offset_micros, 1_000);
        assert_eq!(offset.uncertainty_micros, 100);
        assert!(clock_offset(pong, 99).is_none());
        assert!(
            clock_offset(
                ClockPong {
                    host_sent_at_micros: 1_199,
                    ..pong
                },
                350
            )
            .is_none()
        );
    }

    #[tokio::test]
    async fn small_startup_exchanges_align_clocks_and_agree_on_the_baseline() {
        let server = quinn::Endpoint::server(
            crate::quic::ephemeral_server_config().unwrap(),
            "127.0.0.1:0".parse().unwrap(),
        )
        .unwrap();
        let mut client = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        client.set_default_client_config(crate::quic::opaque_client_config().unwrap());
        let connecting = client
            .connect(server.local_addr().unwrap(), "rustconsole.invalid")
            .unwrap();
        let (player_connection, host_connection) =
            tokio::join!(async { connecting.await.unwrap() }, async {
                server.accept().await.unwrap().await.unwrap()
            },);
        let started = Instant::now();
        let player_side = async {
            let (mut send, mut receive) = player_connection.open_bi().await.unwrap();
            player(&mut send, &mut receive, started).await.unwrap()
        };
        let host_side = async {
            let (mut send, mut receive) = host_connection.accept_bi().await.unwrap();
            host(&mut send, &mut receive, || {
                Ok(50_000_000 + elapsed_micros(started))
            })
            .await
            .unwrap()
        };
        let ((baseline, offset), host_baseline) = tokio::join!(player_side, host_side);
        assert_eq!(baseline, host_baseline);
        assert!(baseline > 0);
        assert!(offset.offset_micros.abs_diff(50_000_000) <= offset.uncertainty_micros);
    }
}

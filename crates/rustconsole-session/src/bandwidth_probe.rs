//! Fixed, bounded, frame-shaped startup-bandwidth probe datagrams.

use bytes::{BufMut, Bytes, BytesMut};
use rustconsole_protocol::wire::{
    BandwidthProbeFinish, BandwidthProbeReport, BandwidthProbeStart, Envelope, envelope,
};
use std::collections::HashSet;
use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const VERSION: u32 = 1;
pub const MAGIC: [u8; 2] = *b"RB";
pub const HEADER_SIZE: usize = 20;
pub const MINIMUM_DATAGRAM_SIZE: usize = HEADER_SIZE + 1;
pub const WARMUP_DURATION: Duration = Duration::from_secs(1);
pub const MEASUREMENT_DURATION: Duration = Duration::from_secs(2);
pub const DATAGRAM_SIZE: usize = 1_200;
const MEASUREMENT_FLAG: u8 = 1;
const MAXIMUM_RECEIVED_DATAGRAMS: usize = 100_000;
const FRAME_PROFILE_LENGTH: u64 = 250;
const KEYFRAME_MULTIPLIER_PER_MILLE: u64 = 7_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProbeDatagram {
    pub probe_id: u64,
    pub sequence: u64,
    pub measurement: bool,
}

impl ProbeDatagram {
    pub fn encode(self, datagram_size: usize) -> Result<Bytes, BandwidthProbeDatagramError> {
        if datagram_size < MINIMUM_DATAGRAM_SIZE {
            return Err(BandwidthProbeDatagramError);
        }
        let mut bytes = BytesMut::with_capacity(datagram_size);
        bytes.extend_from_slice(&MAGIC);
        bytes.put_u8(VERSION as u8);
        bytes.put_u8(if self.measurement {
            MEASUREMENT_FLAG
        } else {
            0
        });
        bytes.put_u64(self.probe_id);
        bytes.put_u64(self.sequence);
        while bytes.len() < datagram_size {
            let offset = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
            bytes.put_u8(
                self.probe_id
                    .wrapping_add(self.sequence)
                    .wrapping_add(offset) as u8,
            );
        }
        Ok(bytes.freeze())
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, BandwidthProbeDatagramError> {
        if bytes.len() < MINIMUM_DATAGRAM_SIZE
            || bytes[..2] != MAGIC
            || u32::from(bytes[2]) != VERSION
            || bytes[3] & !MEASUREMENT_FLAG != 0
        {
            return Err(BandwidthProbeDatagramError);
        }
        Ok(Self {
            probe_id: u64::from_be_bytes(
                bytes[4..12]
                    .try_into()
                    .map_err(|_| BandwidthProbeDatagramError)?,
            ),
            sequence: u64::from_be_bytes(
                bytes[12..20]
                    .try_into()
                    .map_err(|_| BandwidthProbeDatagramError)?,
            ),
            measurement: bytes[3] & MEASUREMENT_FLAG != 0,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BandwidthProbeDatagramError;

impl fmt::Display for BandwidthProbeDatagramError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("malformed bandwidth probe datagram")
    }
}

impl std::error::Error for BandwidthProbeDatagramError {}

pub async fn send(
    connection: &quinn::Connection,
    control_send: &mut quinn::SendStream,
    control_receive: &mut quinn::RecvStream,
    maximum_bits_per_second: u64,
    frames_per_second: u16,
) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
    send_with_configuration(
        connection,
        control_send,
        control_receive,
        maximum_bits_per_second,
        frames_per_second,
        WARMUP_DURATION,
        MEASUREMENT_DURATION,
    )
    .await
}

async fn send_with_configuration(
    connection: &quinn::Connection,
    control_send: &mut quinn::SendStream,
    control_receive: &mut quinn::RecvStream,
    maximum_bits_per_second: u64,
    frames_per_second: u16,
    warmup: Duration,
    measurement: Duration,
) -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
    if maximum_bits_per_second == 0
        || frames_per_second == 0
        || warmup.is_zero()
        || measurement.is_zero()
    {
        return Err("bandwidth probe configuration contains zero".into());
    }
    let maximum_datagram_size = connection
        .max_datagram_size()
        .ok_or("peer does not support required bandwidth-probe datagrams")?;
    let datagram_size = DATAGRAM_SIZE.min(maximum_datagram_size);
    if datagram_size < MINIMUM_DATAGRAM_SIZE {
        return Err("negotiated datagram size is too small for bandwidth probing".into());
    }
    let probe_id = u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_micros())?.max(1);
    crate::quic::write_envelope(
        control_send,
        Envelope {
            body: Some(envelope::Body::BandwidthProbeStart(BandwidthProbeStart {
                probe_id,
                warmup_micros: u64::try_from(warmup.as_micros())?,
                measurement_micros: u64::try_from(measurement.as_micros())?,
                maximum_bits_per_second,
                datagram_size: u32::try_from(datagram_size)?,
            })),
        },
    )
    .await?;

    let started = tokio::time::Instant::now();
    let finished_at = started + warmup + measurement;
    let frame_period = Duration::from_secs(1) / u32::from(frames_per_second);
    let average_frame_bytes = maximum_bits_per_second
        .div_ceil(8 * u64::from(frames_per_second))
        .max(1);
    let datagram_bytes = u64::try_from(datagram_size)?;
    let mut budget = 0_u64;
    let mut frame_index = 0_u64;
    let random_seed = probe_id;
    let mut random_state = probe_id;
    let mut sequence = 0_u64;
    let mut measurement_datagrams_sent = 0_u64;
    let mut measurement_bytes_sent = 0_u64;
    let mut tick = tokio::time::interval(frame_period);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    while tokio::time::Instant::now() < finished_at {
        tick.tick().await;
        let is_measurement =
            tokio::time::Instant::now().saturating_duration_since(started) >= warmup;
        budget = budget.saturating_add(random_frame_bytes(
            average_frame_bytes,
            frame_index,
            random_seed,
            &mut random_state,
        ));
        frame_index = frame_index.saturating_add(1);
        while budget >= datagram_bytes {
            let datagram = ProbeDatagram {
                probe_id,
                sequence,
                measurement: is_measurement,
            }
            .encode(datagram_size)?;
            sequence = sequence.saturating_add(1);
            if !crate::quic::send_media_datagram(connection, datagram, Duration::from_millis(1))
                .await?
            {
                budget = 0;
                break;
            }
            budget -= datagram_bytes;
            if is_measurement {
                measurement_datagrams_sent = measurement_datagrams_sent.saturating_add(1);
                measurement_bytes_sent = measurement_bytes_sent.saturating_add(datagram_bytes);
            }
        }
    }
    crate::quic::write_envelope(
        control_send,
        Envelope {
            body: Some(envelope::Body::BandwidthProbeFinish(BandwidthProbeFinish {
                probe_id,
                measurement_datagrams_sent,
                measurement_bytes_sent,
            })),
        },
    )
    .await?;
    let report = tokio::time::timeout(
        Duration::from_secs(3),
        crate::quic::read_envelope(control_receive),
    )
    .await
    .map_err(|_| "timed out waiting for the bandwidth probe report")??;
    let report = match report.body {
        Some(envelope::Body::BandwidthProbeReport(report)) => report,
        _ => return Err("peer did not return a bandwidth probe report".into()),
    };
    let expected_received_bytes = report
        .measurement_datagrams_received
        .checked_mul(datagram_bytes)
        .ok_or("bandwidth probe byte count overflow")?;
    if report.probe_id != probe_id
        || report.measurement_micros != u64::try_from(measurement.as_micros())?
        || report.measurement_datagrams_received > measurement_datagrams_sent
        || report.measurement_bytes_received != expected_received_bytes
        || report.measurement_bytes_received > measurement_bytes_sent
    {
        return Err("peer returned an invalid bandwidth probe report".into());
    }
    Ok(delivered_bits_per_second(
        report.measurement_bytes_received,
        report.measurement_micros,
    ))
}

fn random_frame_bytes(average: u64, frame_index: u64, seed: u64, state: &mut u64) -> u64 {
    *state ^= state.wrapping_shl(13);
    *state ^= state.wrapping_shr(7);
    *state ^= state.wrapping_shl(17);
    let keyframe_slot = mixed(seed ^ (frame_index / FRAME_PROFILE_LENGTH)) % FRAME_PROFILE_LENGTH;
    let multiplier = if frame_index % FRAME_PROFILE_LENGTH == keyframe_slot {
        KEYFRAME_MULTIPLIER_PER_MILLE
    } else {
        match *state % 996 {
            0..=802 => 920,
            803..=951 => 1_100,
            952..=985 => 1_500,
            _ => 1_800,
        }
    };
    u64::try_from(u128::from(average).saturating_mul(u128::from(multiplier)) / 1_000)
        .unwrap_or(u64::MAX)
        .max(1)
}

fn mixed(mut value: u64) -> u64 {
    value ^= value >> 30;
    value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

pub async fn receive(
    connection: &quinn::Connection,
    control_send: &mut quinn::SendStream,
    control_receive: &mut quinn::RecvStream,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let start = match crate::quic::read_envelope(control_receive).await?.body {
        Some(envelope::Body::BandwidthProbeStart(start)) => start,
        _ => return Err("peer did not start the required bandwidth probe".into()),
    };
    let datagram_size = usize::try_from(start.datagram_size)?;
    if start.probe_id == 0
        || start.warmup_micros == 0
        || start.warmup_micros > 2_000_000
        || start.measurement_micros == 0
        || start.measurement_micros > 3_000_000
        || start.maximum_bits_per_second == 0
        || datagram_size < MINIMUM_DATAGRAM_SIZE
        || datagram_size > connection.max_datagram_size().unwrap_or(0)
    {
        return Err("peer sent invalid bandwidth probe limits".into());
    }
    let probe_deadline = tokio::time::Instant::now()
        + Duration::from_micros(start.warmup_micros.saturating_add(start.measurement_micros))
        + Duration::from_secs(2);
    let mut received_sequences = HashSet::new();
    let finish = loop {
        tokio::select! {
            envelope = crate::quic::read_envelope(control_receive) => {
                match envelope?.body {
                    Some(envelope::Body::BandwidthProbeFinish(finish)) if finish.probe_id == start.probe_id => break finish,
                    _ => return Err("peer sent an invalid bandwidth probe control message".into()),
                }
            }
            datagram = connection.read_datagram() => {
                record_received(&start, &mut received_sequences, &datagram?)?;
            }
            () = tokio::time::sleep_until(probe_deadline) => {
                return Err("bandwidth probe timed out".into());
            }
        }
    };
    let grace = (connection.rtt() * 2).clamp(Duration::from_millis(20), Duration::from_millis(250));
    let grace_deadline = tokio::time::Instant::now() + grace;
    loop {
        match tokio::time::timeout_at(grace_deadline, connection.read_datagram()).await {
            Ok(Ok(datagram)) => record_received(&start, &mut received_sequences, &datagram)?,
            Ok(Err(error)) => return Err(error.into()),
            Err(_) => break,
        }
    }
    let received = u64::try_from(received_sequences.len())?;
    if received > finish.measurement_datagrams_sent {
        return Err("bandwidth probe received more datagrams than the peer sent".into());
    }
    crate::quic::write_envelope(
        control_send,
        Envelope {
            body: Some(envelope::Body::BandwidthProbeReport(BandwidthProbeReport {
                probe_id: start.probe_id,
                measurement_datagrams_received: received,
                measurement_bytes_received: received.saturating_mul(u64::from(start.datagram_size)),
                measurement_micros: start.measurement_micros,
            })),
        },
    )
    .await?;
    Ok(())
}

fn record_received(
    start: &BandwidthProbeStart,
    received_sequences: &mut HashSet<u64>,
    datagram: &[u8],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let probe = ProbeDatagram::decode(datagram)?;
    if probe.probe_id != start.probe_id || datagram.len() != usize::try_from(start.datagram_size)? {
        return Err("peer sent an inconsistent bandwidth probe datagram".into());
    }
    if probe.measurement {
        if received_sequences.len() >= MAXIMUM_RECEIVED_DATAGRAMS {
            return Err("bandwidth probe exceeded the receive bound".into());
        }
        received_sequences.insert(probe.sequence);
    }
    Ok(())
}

fn delivered_bits_per_second(received_bytes: u64, measurement_micros: u64) -> u64 {
    if measurement_micros == 0 {
        return 0;
    }
    u64::try_from(
        u128::from(received_bytes)
            .saturating_mul(8_000_000)
            .checked_div(u128::from(measurement_micros))
            .unwrap_or(0),
    )
    .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_datagram_round_trips_at_fixed_size() {
        let expected = ProbeDatagram {
            probe_id: 7,
            sequence: 19,
            measurement: true,
        };
        let bytes = expected.encode(1_200).unwrap();

        assert_eq!(bytes.len(), 1_200);
        assert_eq!(ProbeDatagram::decode(&bytes), Ok(expected));
    }

    #[test]
    fn probe_datagram_rejects_wrong_kind_and_flags() {
        let mut bytes = ProbeDatagram {
            probe_id: 1,
            sequence: 2,
            measurement: false,
        }
        .encode(MINIMUM_DATAGRAM_SIZE)
        .unwrap()
        .to_vec();
        bytes[0] = b'V';
        assert!(ProbeDatagram::decode(&bytes).is_err());
        bytes[0] = MAGIC[0];
        bytes[3] = 2;
        assert!(ProbeDatagram::decode(&bytes).is_err());
    }

    #[test]
    fn random_frame_profile_has_one_large_burst_per_window() {
        let average = 10_000_u64;
        let seed = 17;
        let mut state = seed;
        let frames = (0..FRAME_PROFILE_LENGTH)
            .map(|frame| random_frame_bytes(average, frame, seed, &mut state))
            .collect::<Vec<_>>();

        assert_eq!(
            frames.iter().filter(|bytes| **bytes == average * 7).count(),
            1
        );
        let total = frames.iter().copied().sum::<u64>();
        assert!(total >= average * 240);
        assert!(total <= average * 260);
    }

    #[test]
    fn random_frame_profile_changes_with_the_probe_seed() {
        let mut first_state = 17;
        let mut second_state = 19;
        let first = (0..FRAME_PROFILE_LENGTH)
            .map(|frame| random_frame_bytes(10_000, frame, 17, &mut first_state))
            .collect::<Vec<_>>();
        let second = (0..FRAME_PROFILE_LENGTH)
            .map(|frame| random_frame_bytes(10_000, frame, 19, &mut second_state))
            .collect::<Vec<_>>();

        assert_ne!(first, second);
    }

    #[tokio::test]
    async fn loopback_probe_reports_delivered_capacity() {
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
        let (client_connection, server_connection) =
            tokio::join!(async { connecting.await.unwrap() }, async {
                server.accept().await.unwrap().await.unwrap()
            },);
        let player = async {
            let (mut send, mut control_receive) = client_connection.accept_bi().await.unwrap();
            receive(&client_connection, &mut send, &mut control_receive)
                .await
                .unwrap();
        };
        let host = async {
            let (mut send, mut control_receive) = server_connection.open_bi().await.unwrap();
            send_with_configuration(
                &server_connection,
                &mut send,
                &mut control_receive,
                8_000_000,
                144,
                Duration::from_millis(100),
                Duration::from_millis(200),
            )
            .await
            .unwrap()
        };

        let ((), capacity) = tokio::join!(player, host);

        assert!(capacity > 0);
        // The 200 ms window can include one 7x keyframe burst, with
        // other frames up to 1.8x average. Include a boundary frame
        // and the partial datagram budget carried from warmup.
        let maximum_frames = 30_u64;
        let average_frame_bytes = 8_000_000_u64.div_ceil(8 * 144);
        let maximum_bytes = average_frame_bytes * (7_000 + (maximum_frames - 1) * 1_800) / 1_000
            + DATAGRAM_SIZE as u64;
        assert!(capacity <= maximum_bytes * 8 * 5);
    }
}

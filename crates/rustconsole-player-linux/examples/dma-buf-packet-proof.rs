use rustconsole_player_linux::{run_dma_buf_packet_proof, run_dma_buf_proof};
use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args_os().skip(1);
    let packet_path = arguments.next().ok_or("missing AV1 packet path")?;
    let report_path = arguments.next().ok_or("missing report path")?;
    if arguments.next().is_some() {
        return Err("unexpected extra argument".into());
    }
    if packet_path == "--fixture" {
        run_dma_buf_proof(Path::new(&report_path))
    } else {
        let packet = std::fs::read(packet_path)?;
        run_dma_buf_packet_proof(&packet, Path::new(&report_path))
    }
}

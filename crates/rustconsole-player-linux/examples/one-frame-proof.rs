use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args().skip(1);
    let address = arguments
        .next()
        .ok_or("expected <host-address> <report-path>")?;
    let report = arguments
        .next()
        .ok_or("expected <host-address> <report-path>")?;
    if arguments.next().is_some() {
        return Err("expected exactly <host-address> <report-path>".into());
    }
    rustconsole_player_linux::run_one_frame_proof(&address, Path::new(&report))
}

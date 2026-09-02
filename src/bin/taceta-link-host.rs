use std::io::{self, BufReader, BufWriter};
use taceta::browser_harness::{
    ConnectionManager, Envelope, MessageType, ProtocolError, default_socket_path, read_frame,
    validate_envelope, write_frame,
};

fn main() -> Result<(), ProtocolError> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut input = BufReader::new(stdin.lock());
    let mut output = BufWriter::new(stdout.lock());
    loop {
        let request: Envelope<serde_json::Value> = match read_frame(&mut input) {
            Ok(value) => value,
            Err(ProtocolError::Io(error)) if error.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(error) => return Err(error),
        };
        if let Err(error) = validate_envelope(&request, env!("CARGO_PKG_VERSION")) {
            eprintln!("taceta-link-host: {error}");
            return Err(error);
        }
        if request.message_type != MessageType::Request {
            return Err(ProtocolError::CorrelationMismatch);
        }
        let mut manager = ConnectionManager::connect(
            &default_socket_path()?,
            env!("CARGO_PKG_VERSION"),
            request.session_id,
        )?;
        manager.send(&request)?;
        let response: Envelope<serde_json::Value> = manager.receive()?;
        write_frame(&mut output, &response)?;
    }
    Ok(())
}

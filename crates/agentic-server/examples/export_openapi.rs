use std::io::{self, Write};

use agentic_server::openapi::ApiDoc;
use utoipa::OpenApi;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    serde_json::to_writer_pretty(&mut output, &ApiDoc::openapi())?;
    writeln!(output)?;
    Ok(())
}

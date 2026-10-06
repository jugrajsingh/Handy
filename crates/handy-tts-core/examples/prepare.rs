fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).ok_or("input path required")?;
    let source = std::fs::read_to_string(path)?;
    let mut cursor = handy_tts_core::text::TextCursor::new(&source, false)?;
    let mut chunks = Vec::new();
    while let Some(chunk) = cursor.next_chunk()? {
        chunks.push(chunk);
    }
    println!("{}", serde_json::to_string_pretty(&chunks)?);
    Ok(())
}

use std::error::Error;
use vergen::{Build, Emitter};

fn main() -> Result<(), Box<dyn Error>> {
    let build = Build::all_build();
    Emitter::default().add_instructions(&build)?.emit()?;
    Ok(())
}

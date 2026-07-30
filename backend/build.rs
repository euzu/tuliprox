use std::error::Error;
use vergen::{BuildBuilder, Emitter};

fn main() -> Result<(), Box<dyn Error>> {
    let build = BuildBuilder::all_build()?;
    Emitter::default().add_instructions(&build)?.emit()?;
    Ok(())
}

use rand::RngCore;

use crate::{CONFIG_FILE, database::POSTGRES};

pub async fn new_puzzle() -> String {
    let mut bts = [0u8; 20];
    rand::rng().fill_bytes(&mut bts);
    hex::encode(bts)
}

pub async fn verify_puzzle_solution(puzzle: &str, solution: &str) -> anyhow::Result<()> {
    geph5_broker_protocol::puzzle::verify_puzzle_solution(
        puzzle,
        CONFIG_FILE.wait().puzzle_difficulty,
        solution,
    )?;
    // deduplicate on insert
    sqlx::query("insert into used_puzzles values ($1)")
        .bind(puzzle)
        .execute(&*POSTGRES)
        .await?;
    Ok(())
}

use rand::RngCore;

use super::POSTGRES;
use crate::CONFIG_FILE;

pub async fn new_puzzle() -> String {
    let mut bts = [0u8; 20];
    rand::thread_rng().fill_bytes(&mut bts);
    hex::encode(bts)
}

pub async fn verify_puzzle_solution(puzzle: &str, solution: &str) -> anyhow::Result<()> {
    geph5_broker_protocol::puzzle::verify_puzzle_solution(
        puzzle,
        CONFIG_FILE.wait().puzzle_difficulty,
        solution,
    )?;
    // Insert with ON CONFLICT DO NOTHING; 0 rows affected means this puzzle was already used.
    // Requires a UNIQUE constraint on used_puzzles(puzzle).
    let res = sqlx::query("insert into used_puzzles values ($1) on conflict do nothing")
        .bind(puzzle)
        .execute(&*POSTGRES)
        .await?;
    if res.rows_affected() == 0 {
        anyhow::bail!("puzzle already used");
    }
    Ok(())
}

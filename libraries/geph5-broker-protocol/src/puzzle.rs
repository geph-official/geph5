use anyhow::Context;
use base64::{engine::general_purpose::STANDARD_NO_PAD, Engine as _};

/// Generates the solution to a particular puzzle, calling a callback for progress.
pub fn solve_puzzle(puzzle: &str, difficulty: u16, on_progress: impl Fn(f64)) -> String {
    STANDARD_NO_PAD.encode(
        melpow::Proof::generate_with_progress(
            puzzle.as_bytes(),
            difficulty as _,
            on_progress,
            Blake3HashFunction,
        )
        .to_bytes(),
    )
}

/// Verify a puzzle solution
pub fn verify_puzzle_solution(puzzle: &str, difficulty: u16, solution: &str) -> anyhow::Result<()> {
    let solution = STANDARD_NO_PAD.decode(&solution)?;
    let solution = melpow::Proof::from_bytes(&solution).context("invalid solution format")?;
    if !solution.verify(puzzle.as_bytes(), difficulty as _, Blake3HashFunction) {
        anyhow::bail!("invalid solution to puzzle")
    }
    Ok(())
}

struct Blake3HashFunction;

impl melpow::HashFunction for Blake3HashFunction {
    fn hash(&self, b: &[u8], k: &[u8]) -> melpow::SVec<u8> {
        melpow::SVec::from_slice(blake3::keyed_hash(blake3::hash(k).as_bytes(), b).as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_progress_callback_invoked() {
        let puzzle = "progress test";
        let difficulty = 15;

        let res = solve_puzzle(puzzle, difficulty, |progress| {
            dbg!(progress);
        });
        dbg!(res.len());
    }
}

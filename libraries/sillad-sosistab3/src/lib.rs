mod handshake;

pub fn add(left: usize, right: usize) -> usize {
    left + right
}

#[derive(Clone, Copy)]
pub struct Cookie([u8; 32]);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_works() {
        let result = add(2, 2);
        assert_eq!(result, 4);
    }
}

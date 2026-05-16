use thiserror::Error;

#[derive(Debug, Error)]
pub enum FibError {
    #[error("n too large (max 186, got {0})")]
    TooLarge(u32),
}

/// Iterative fibonacci. u128 overflows past F(186), so we cap there.
pub fn compute(n: u32) -> Result<u128, FibError> {
    if n > 186 {
        return Err(FibError::TooLarge(n));
    }
    if n < 2 {
        return Ok(n as u128);
    }
    let mut a: u128 = 0;
    let mut b: u128 = 1;
    for _ in 2..=n {
        let next = a + b;
        a = b;
        b = next;
    }
    Ok(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small() {
        assert_eq!(compute(0).unwrap(), 0);
        assert_eq!(compute(1).unwrap(), 1);
        assert_eq!(compute(10).unwrap(), 55);
        assert_eq!(compute(20).unwrap(), 6765);
    }

    #[test]
    fn too_large() {
        assert!(compute(187).is_err());
    }
}

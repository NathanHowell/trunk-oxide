//! GF(2^6) with the primitive polynomial x^6 + x + 1: the field of the NID's
//! BCH code and of every P25 Reed–Solomon code (hexbit symbols).

use std::fmt;
use std::ops::{Add, AddAssign, Div, Mul, MulAssign, Sub};

const ORDER: usize = 63;

const fn tables() -> ([u8; 2 * ORDER], [u8; 64]) {
    let mut exp = [0u8; 2 * ORDER];
    let mut log = [0u8; 64];
    let mut x: u8 = 1;
    let mut i = 0;
    while i < ORDER {
        exp[i] = x;
        exp[i + ORDER] = x;
        #[allow(clippy::cast_possible_truncation)]
        {
            log[x as usize] = i as u8;
        }
        x <<= 1;
        if x & 0x40 != 0 {
            x ^= 0x43;
        }
        i += 1;
    }
    (exp, log)
}

const TABLES: ([u8; 2 * ORDER], [u8; 64]) = tables();
const EXP: [u8; 2 * ORDER] = TABLES.0;
const LOG: [u8; 64] = TABLES.1;

/// An element of GF(64). Addition is XOR.
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Gf64(u8);

impl Gf64 {
    pub const ZERO: Self = Self(0);
    pub const ONE: Self = Self(1);

    /// The low six bits of `v` (a hexbit).
    #[must_use]
    pub const fn new(v: u8) -> Self {
        Self(v & 63)
    }

    pub const fn get(self) -> u8 {
        self.0
    }

    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    /// α^e for any exponent.
    #[must_use]
    pub const fn alpha(e: usize) -> Self {
        Self(EXP[e % ORDER])
    }

    /// `log_α` of a non-zero element.
    pub const fn log(self) -> Option<usize> {
        if self.0 == 0 {
            None
        } else {
            Some(LOG[self.0 as usize] as usize)
        }
    }

    #[must_use]
    pub const fn inv(self) -> Self {
        match self.log() {
            Some(l) => Self(EXP[(ORDER - l) % ORDER]),
            None => panic!("inverse of zero"),
        }
    }

    /// `self + o`, usable in const contexts (the `+` operator delegates here).
    #[must_use]
    pub const fn plus(self, o: Self) -> Self {
        Self(self.0 ^ o.0)
    }

    /// `self · o`, usable in const contexts.
    #[must_use]
    pub const fn times(self, o: Self) -> Self {
        match (self.log(), o.log()) {
            (Some(a), Some(b)) => Self(EXP[a + b]),
            _ => Self::ZERO,
        }
    }

    #[must_use]
    pub const fn pow(self, e: usize) -> Self {
        match self.log() {
            Some(l) => Self(EXP[(l * (e % ORDER)) % ORDER]),
            None if e == 0 => Self::ONE,
            None => Self::ZERO,
        }
    }
}

impl fmt::Debug for Gf64 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Add for Gf64 {
    type Output = Self;
    fn add(self, o: Self) -> Self {
        self.plus(o)
    }
}

impl Sub for Gf64 {
    type Output = Self;
    #[allow(clippy::suspicious_arithmetic_impl)] // characteristic 2
    fn sub(self, o: Self) -> Self {
        Self(self.0 ^ o.0)
    }
}

impl AddAssign for Gf64 {
    fn add_assign(&mut self, o: Self) {
        *self = self.plus(o);
    }
}

impl Mul for Gf64 {
    type Output = Self;
    fn mul(self, o: Self) -> Self {
        self.times(o)
    }
}

impl MulAssign for Gf64 {
    fn mul_assign(&mut self, o: Self) {
        *self = *self * o;
    }
}

impl Div for Gf64 {
    type Output = Self;
    #[allow(clippy::suspicious_arithmetic_impl)]
    fn div(self, o: Self) -> Self {
        self * o.inv()
    }
}

/// Evaluate a polynomial (lowest degree first) at `x`.
pub fn eval(poly: &[Gf64], x: Gf64) -> Gf64 {
    poly.iter().rev().fold(Gf64::ZERO, |acc, &c| acc * x + c)
}

/// A polynomial over GF(64), lowest degree first, never with a zero leading
/// coefficient (the zero polynomial is empty).
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct Poly(Vec<Gf64>);

impl Poly {
    #[must_use]
    pub fn new(mut coefficients: Vec<Gf64>) -> Self {
        while coefficients.last().is_some_and(|c| c.is_zero()) {
            coefficients.pop();
        }
        Self(coefficients)
    }

    #[must_use]
    pub fn one() -> Self {
        Self(vec![Gf64::ONE])
    }

    /// `c · x^d`.
    #[must_use]
    pub fn monomial(c: Gf64, d: usize) -> Self {
        let mut v = vec![Gf64::ZERO; d + 1];
        v[d] = c;
        Self::new(v)
    }

    /// `1 + a·x`, the locator factor of a root at a⁻¹.
    #[must_use]
    pub fn locator(a: Gf64) -> Self {
        Self::new(vec![Gf64::ONE, a])
    }

    pub const fn degree(&self) -> Option<usize> {
        self.0.len().checked_sub(1)
    }

    pub fn coef(&self, i: usize) -> Gf64 {
        self.0.get(i).copied().unwrap_or(Gf64::ZERO)
    }

    pub fn coefficients(&self) -> &[Gf64] {
        &self.0
    }

    pub fn eval(&self, x: Gf64) -> Gf64 {
        eval(&self.0, x)
    }

    /// Formal derivative: in characteristic 2 only the odd terms survive.
    #[must_use]
    pub fn derivative(&self) -> Self {
        Self::new(
            self.0
                .iter()
                .enumerate()
                .skip(1)
                .map(|(i, &c)| if i % 2 == 1 { c } else { Gf64::ZERO })
                .collect(),
        )
    }

    /// `self mod x^n`.
    #[must_use]
    pub fn truncate(&self, n: usize) -> Self {
        Self::new(self.0.iter().copied().take(n).collect())
    }

    #[must_use]
    pub fn scale(&self, c: Gf64) -> Self {
        Self::new(self.0.iter().map(|&a| a * c).collect())
    }

    /// `self · x^d`.
    #[must_use]
    pub fn shift(&self, d: usize) -> Self {
        let mut v = vec![Gf64::ZERO; d];
        v.extend_from_slice(&self.0);
        Self::new(v)
    }
}

impl FromIterator<Gf64> for Poly {
    fn from_iter<I: IntoIterator<Item = Gf64>>(iter: I) -> Self {
        Self::new(iter.into_iter().collect())
    }
}

impl Add for &Poly {
    type Output = Poly;
    fn add(self, o: &Poly) -> Poly {
        let n = self.0.len().max(o.0.len());
        (0..n).map(|i| self.coef(i) + o.coef(i)).collect()
    }
}

impl Mul for &Poly {
    type Output = Poly;
    fn mul(self, o: &Poly) -> Poly {
        if self.0.is_empty() || o.0.is_empty() {
            return Poly::default();
        }
        let mut v = vec![Gf64::ZERO; self.0.len() + o.0.len() - 1];
        for (i, &a) in self.0.iter().enumerate() {
            for (j, &b) in o.0.iter().enumerate() {
                v[i + j] += a * b;
            }
        }
        Poly::new(v)
    }
}

impl std::ops::Rem for &Poly {
    type Output = Poly;
    fn rem(self, divisor: &Poly) -> Poly {
        let d = divisor.degree().expect("division by the zero polynomial");
        let lead = divisor.0[d].inv();
        let mut r = self.0.clone();
        while r.len() > d {
            let top = r.len() - 1;
            let q = r[top] * lead;
            if !q.is_zero() {
                for (i, &c) in divisor.0.iter().enumerate() {
                    r[top - d + i] += q * c;
                }
            }
            r.pop();
        }
        Poly::new(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_axioms_hold() {
        for a in 1..64u8 {
            let a = Gf64::new(a);
            assert_eq!(a * a.inv(), Gf64::ONE);
            assert_eq!(a / a, Gf64::ONE);
            assert_eq!(a + a, Gf64::ZERO);
            assert_eq!(a.pow(63), Gf64::ONE);
            for b in [1u8, 2, 17, 63] {
                let b = Gf64::new(b);
                assert_eq!((a * b) / b, a);
            }
        }
        assert_eq!(Gf64::alpha(6), Gf64::new(3)); // x^6 = x + 1
    }

    #[test]
    fn polynomial_ring_identities() {
        let a = Poly::new(vec![Gf64::new(3), Gf64::ZERO, Gf64::new(7)]);
        let b = Poly::locator(Gf64::alpha(5));
        let p = &a * &b;
        assert_eq!(p.degree(), Some(3));
        assert_eq!(&p % &b, Poly::default());
        assert_eq!(&(&p + &a) % &b, &a % &b);
        // (fg)' = f'g + fg'
        assert_eq!(
            p.derivative(),
            &(&a.derivative() * &b) + &(&a * &b.derivative())
        );
        // the locator of a vanishes at a⁻¹
        assert!(b.eval(Gf64::alpha(5).inv()).is_zero());
    }

    #[test]
    fn horner_evaluation() {
        // 1 + x + x^2 at α: 1 + α + α^2
        let p = [Gf64::ONE, Gf64::ONE, Gf64::ONE];
        assert_eq!(eval(&p, Gf64::alpha(1)), Gf64::new(1 ^ 2 ^ 4));
    }
}

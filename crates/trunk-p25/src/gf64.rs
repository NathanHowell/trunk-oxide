//! GF(2^6) with the primitive polynomial x^6 + x + 1: the field of the NID's
//! BCH code and of every P25 Reed–Solomon code (hexbit symbols).

use std::fmt;
use std::ops::{Add, AddAssign, Div, Mul, MulAssign, Sub};

const ORDER: usize = 63;

/// The field's primitive polynomial, x⁶ + x + 1 (BBAC Annex A; BAAA).
pub const PRIMITIVE: u8 = 0x43;
/// x⁶, the bit the primitive polynomial reduces.
const DEGREE_BIT: u8 = 1 << 6;

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
        if x & DEGREE_BIT != 0 {
            x ^= PRIMITIVE;
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

/// The most coefficients a [`Poly`] holds.
///
/// Berlekamp–Massey's auxiliary polynomial reaches twice the parity count
/// (a code has at most 62 parity symbols) before the checks that reject a
/// word; a product that is kept is far shorter.
const CAPACITY: usize = 128;

/// A polynomial over GF(64), lowest degree first, never with a zero leading
/// coefficient (the zero polynomial is empty).
///
/// It lives on the stack, so the decoders that build many of them in a word
/// allocate nothing; an operation that would outgrow [`CAPACITY`] panics.
#[derive(Clone, Copy)]
pub struct Poly {
    /// Zero beyond `len`.
    c: [Gf64; CAPACITY],
    len: usize,
}

impl Default for Poly {
    fn default() -> Self {
        Self::ZERO
    }
}

impl PartialEq for Poly {
    fn eq(&self, o: &Self) -> bool {
        self.coefficients() == o.coefficients()
    }
}

impl Eq for Poly {}

impl fmt::Debug for Poly {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.coefficients()).finish()
    }
}

impl Poly {
    /// The zero polynomial.
    pub const ZERO: Self = Self {
        c: [Gf64::ZERO; CAPACITY],
        len: 0,
    };

    /// Drops leading zero coefficients.
    const fn trimmed(mut self) -> Self {
        while self.len > 0 && self.c[self.len - 1].is_zero() {
            self.len -= 1;
        }
        self
    }

    #[must_use]
    pub fn new(coefficients: &[Gf64]) -> Self {
        let mut p = Self::ZERO;
        p.c[..coefficients.len()].copy_from_slice(coefficients);
        p.len = coefficients.len();
        p.trimmed()
    }

    #[must_use]
    pub const fn one() -> Self {
        let mut p = Self::ZERO;
        p.c[0] = Gf64::ONE;
        p.len = 1;
        p
    }

    /// `c · x^d`.
    #[must_use]
    pub const fn monomial(c: Gf64, d: usize) -> Self {
        let mut p = Self::ZERO;
        p.c[d] = c;
        p.len = d + 1;
        p.trimmed()
    }

    /// `1 + a·x`, the locator factor of a root at a⁻¹.
    #[must_use]
    pub const fn locator(a: Gf64) -> Self {
        let mut p = Self::ZERO;
        p.c[0] = Gf64::ONE;
        p.c[1] = a;
        p.len = 2;
        p.trimmed()
    }

    pub const fn degree(&self) -> Option<usize> {
        self.len.checked_sub(1)
    }

    pub fn coef(&self, i: usize) -> Gf64 {
        // zero beyond `len`, so no bounds test of that
        self.c.get(i).copied().unwrap_or(Gf64::ZERO)
    }

    pub fn coefficients(&self) -> &[Gf64] {
        &self.c[..self.len]
    }

    pub fn eval(&self, x: Gf64) -> Gf64 {
        eval(self.coefficients(), x)
    }

    /// Formal derivative: in characteristic 2 only the odd terms survive.
    #[must_use]
    pub fn derivative(&self) -> Self {
        let mut p = Self::ZERO;
        for i in (1..self.len).step_by(2) {
            p.c[i - 1] = self.c[i];
        }
        p.len = self.len.saturating_sub(1);
        p.trimmed()
    }

    /// `self mod x^n`.
    #[must_use]
    pub fn truncate(&self, n: usize) -> Self {
        let mut p = *self;
        if n < p.len {
            p.c[n..p.len].fill(Gf64::ZERO);
            p.len = n;
        }
        p.trimmed()
    }

    #[must_use]
    pub fn scale(&self, c: Gf64) -> Self {
        let mut p = *self;
        p.c[..p.len].iter_mut().for_each(|a| *a *= c);
        p.trimmed()
    }

    /// `self · x^d`.
    #[must_use]
    pub fn shift(&self, d: usize) -> Self {
        if self.len == 0 {
            return Self::ZERO;
        }
        let mut p = Self::ZERO;
        p.c[d..d + self.len].copy_from_slice(self.coefficients());
        p.len = d + self.len;
        p
    }

    /// `(self · o) mod x^n`, without forming the terms the reduction drops.
    #[must_use]
    pub fn mul_truncated(&self, o: &Self, n: usize) -> Self {
        let mut p = Self::ZERO;
        for (i, &a) in self.coefficients().iter().enumerate().take(n) {
            for (j, &b) in o.coefficients().iter().enumerate().take(n - i) {
                p.c[i + j] += a * b;
            }
        }
        p.len = (self.len + o.len).saturating_sub(1).min(n);
        p.trimmed()
    }
}

impl FromIterator<Gf64> for Poly {
    fn from_iter<I: IntoIterator<Item = Gf64>>(iter: I) -> Self {
        let mut p = Self::ZERO;
        for c in iter {
            p.c[p.len] = c;
            p.len += 1;
        }
        p.trimmed()
    }
}

impl Add for &Poly {
    type Output = Poly;
    fn add(self, o: &Poly) -> Poly {
        let mut p = *self;
        for (a, &b) in p.c.iter_mut().zip(o.coefficients()) {
            *a += b;
        }
        p.len = self.len.max(o.len);
        p.trimmed()
    }
}

impl Mul for &Poly {
    type Output = Poly;
    fn mul(self, o: &Poly) -> Poly {
        if self.len == 0 || o.len == 0 {
            return Poly::ZERO;
        }
        let mut p = Poly::ZERO;
        for (i, &a) in self.coefficients().iter().enumerate() {
            for (j, &b) in o.coefficients().iter().enumerate() {
                p.c[i + j] += a * b;
            }
        }
        p.len = self.len + o.len - 1;
        p.trimmed()
    }
}

impl std::ops::Rem for &Poly {
    type Output = Poly;
    fn rem(self, divisor: &Poly) -> Poly {
        let d = divisor.degree().expect("division by the zero polynomial");
        let lead = divisor.c[d].inv();
        let mut r = *self;
        while r.len > d {
            let top = r.len - 1;
            let q = r.c[top] * lead;
            if !q.is_zero() {
                for (i, &c) in divisor.coefficients().iter().enumerate() {
                    r.c[top - d + i] += q * c;
                }
            }
            r.c[top] = Gf64::ZERO;
            r.len -= 1;
        }
        r.trimmed()
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
        let a = Poly::new(&[Gf64::new(3), Gf64::ZERO, Gf64::new(7)]);
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

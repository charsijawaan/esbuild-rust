// Port of upstream internal/helpers/float.go.

/// Wrapper that keeps each floating-point operation explicit and rounded.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct F64 {
    value: f64,
}

impl F64 {
    #[must_use]
    pub const fn new(value: f64) -> Self {
        Self { value }
    }

    #[must_use]
    pub const fn value(self) -> f64 {
        self.value
    }

    #[must_use]
    pub fn is_nan(self) -> bool {
        self.value.is_nan()
    }

    #[must_use]
    pub fn negated(self) -> Self {
        Self::new(-self.value)
    }

    #[must_use]
    pub fn abs(self) -> Self {
        Self::new(self.value.abs())
    }

    #[must_use]
    pub fn sin(self) -> Self {
        Self::new(self.value.sin())
    }

    #[must_use]
    pub fn cos(self) -> Self {
        Self::new(self.value.cos())
    }

    #[must_use]
    pub fn log2(self) -> Self {
        Self::new(self.value.log2())
    }

    #[must_use]
    pub fn round(self) -> Self {
        Self::new(self.value.round())
    }

    #[must_use]
    pub fn floor(self) -> Self {
        Self::new(self.value.floor())
    }

    #[must_use]
    pub fn ceil(self) -> Self {
        Self::new(self.value.ceil())
    }

    #[must_use]
    pub fn squared(self) -> Self {
        self.mul(self)
    }

    #[must_use]
    pub fn cubed(self) -> Self {
        self.mul(self).mul(self)
    }

    #[must_use]
    pub fn sqrt(self) -> Self {
        Self::new(self.value.sqrt())
    }

    #[must_use]
    pub fn cbrt(self) -> Self {
        Self::new(self.value.cbrt())
    }

    #[must_use]
    #[allow(clippy::should_implement_trait)]
    pub fn add(self, other: Self) -> Self {
        Self::new(self.value + other.value)
    }

    #[must_use]
    pub fn add_const(self, other: f64) -> Self {
        Self::new(self.value + other)
    }

    #[must_use]
    #[allow(clippy::should_implement_trait)]
    pub fn sub(self, other: Self) -> Self {
        Self::new(self.value - other.value)
    }

    #[must_use]
    pub fn sub_const(self, other: f64) -> Self {
        Self::new(self.value - other)
    }

    #[must_use]
    #[allow(clippy::should_implement_trait)]
    pub fn mul(self, other: Self) -> Self {
        Self::new(self.value * other.value)
    }

    #[must_use]
    pub fn mul_const(self, other: f64) -> Self {
        Self::new(self.value * other)
    }

    #[must_use]
    #[allow(clippy::should_implement_trait)]
    pub fn div(self, other: Self) -> Self {
        Self::new(self.value / other.value)
    }

    #[must_use]
    pub fn div_const(self, other: f64) -> Self {
        Self::new(self.value / other)
    }

    #[must_use]
    pub fn pow(self, other: Self) -> Self {
        Self::new(self.value.powf(other.value))
    }

    #[must_use]
    pub fn pow_const(self, other: f64) -> Self {
        Self::new(self.value.powf(other))
    }

    #[must_use]
    pub fn atan2(self, other: Self) -> Self {
        Self::new(self.value.atan2(other.value))
    }

    #[must_use]
    pub fn with_sign_from(self, other: Self) -> Self {
        Self::new(self.value.copysign(other.value))
    }
}

#[must_use]
pub fn min2(a: F64, b: F64) -> F64 {
    F64::new(a.value.min(b.value))
}

#[must_use]
pub fn max2(a: F64, b: F64) -> F64 {
    F64::new(a.value.max(b.value))
}

#[must_use]
pub fn min3(a: F64, b: F64, c: F64) -> F64 {
    F64::new(a.value.min(b.value).min(c.value))
}

#[must_use]
pub fn max3(a: F64, b: F64, c: F64) -> F64 {
    F64::new(a.value.max(b.value).max(c.value))
}

#[must_use]
pub fn lerp(a: F64, b: F64, t: F64) -> F64 {
    b.sub(a).mul(t).add(a)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp)]

    use super::{F64, lerp, max3, min3};

    #[test]
    fn arithmetic_operations_match_the_upstream_shape() {
        let two = F64::new(2.0);
        let three = F64::new(3.0);
        assert_eq!(two.squared().value(), 4.0);
        assert_eq!(two.cubed().value(), 8.0);
        assert_eq!(three.sub(two).value(), 1.0);
        assert_eq!(lerp(two, F64::new(4.0), F64::new(0.25)).value(), 2.5);
        assert_eq!(min3(three, two, F64::new(4.0)), two);
        assert_eq!(max3(three, two, F64::new(4.0)).value(), 4.0);
    }
}

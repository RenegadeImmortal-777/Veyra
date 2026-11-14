use std::cmp::Ordering;
use std::f64::consts::PI;

pub const BASE: u64 = 1_000_000_000;
const BASE_DIGITS: usize = 9;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BigInt {
    pub neg: bool,
    pub limbs: Vec<u32>,
}

impl BigInt {
    pub fn zero() -> Self {
        Self {
            neg: false,
            limbs: Vec::new(),
        }
    }

    pub fn one() -> Self {
        Self {
            neg: false,
            limbs: vec![1],
        }
    }

    pub fn from_u64(mut n: u64) -> Self {
        if n == 0 {
            return Self::zero();
        }
        let mut limbs = Vec::new();
        while n > 0 {
            limbs.push((n % BASE) as u32);
            n /= BASE;
        }
        Self { neg: false, limbs }
    }

    fn trim(&mut self) {
        while matches!(self.limbs.last(), Some(&0)) {
            self.limbs.pop();
        }
        if self.limbs.is_empty() {
            self.neg = false;
        }
    }

    pub fn is_zero(&self) -> bool {
        self.limbs.is_empty()
    }

    pub fn abs(&self) -> Self {
        Self {
            neg: false,
            limbs: self.limbs.clone(),
        }
    }

    pub fn neg(&self) -> Self {
        if self.is_zero() {
            return Self::zero();
        }
        Self {
            neg: !self.neg,
            limbs: self.limbs.clone(),
        }
    }

    fn mul_add_u32_in_place(&mut self, mul: u64, add: u64) {
        let mut carry = add;
        for limb in &mut self.limbs {
            let v = (*limb as u64) * mul + carry;
            *limb = (v % BASE) as u32;
            carry = v / BASE;
        }
        while carry > 0 {
            self.limbs.push((carry % BASE) as u32);
            carry /= BASE;
        }
        self.trim();
    }

    fn div_rem_u64_mag(&self, divisor: u64) -> (Self, u64) {
        if divisor == 0 || self.is_zero() {
            return (Self::zero(), 0);
        }
        let mut q = vec![0u32; self.limbs.len()];
        let mut rem: u128 = 0;
        let d = divisor as u128;
        let base = BASE as u128;
        for i in (0..self.limbs.len()).rev() {
            let cur = rem * base + self.limbs[i] as u128;
            q[i] = (cur / d) as u32;
            rem = cur % d;
        }
        let mut out = Self {
            neg: false,
            limbs: q,
        };
        out.trim();
        (out, rem as u64)
    }

    pub fn from_str_radix_auto(s: &str) -> Result<Self, String> {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return Err("empty integer string".into());
        }
        let (neg, rest) = if let Some(r) = trimmed.strip_prefix('-') {
            (true, r)
        } else if let Some(r) = trimmed.strip_prefix('+') {
            (false, r)
        } else {
            (false, trimmed)
        };
        let rest = rest.trim();
        if rest.is_empty() {
            return Err(format!("invalid integer literal: {s:?}"));
        }

        if let Some(hex) = rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X")) {
            return Self::from_digits_radix(hex, 16, neg, s);
        }
        if let Some(bin) = rest.strip_prefix("0b").or_else(|| rest.strip_prefix("0B")) {
            return Self::from_digits_radix(bin, 2, neg, s);
        }
        if let Some(oct) = rest.strip_prefix("0o").or_else(|| rest.strip_prefix("0O")) {
            return Self::from_digits_radix(oct, 8, neg, s);
        }

        let clean: String = rest.chars().filter(|&c| c != '_').collect();
        if clean.is_empty() || !clean.bytes().all(|b| b.is_ascii_digit()) {
            return Err(format!("invalid decimal integer literal: {s:?}"));
        }
        let stripped = clean.trim_start_matches('0');
        if stripped.is_empty() {
            return Ok(Self::zero());
        }
        let mut limbs = Vec::with_capacity(stripped.len().div_ceil(BASE_DIGITS));
        let mut end = stripped.len();
        while end > 0 {
            let start = end.saturating_sub(BASE_DIGITS);
            let chunk = stripped[start..end]
                .parse::<u32>()
                .map_err(|_| format!("invalid integer chunk in {s:?}"))?;
            limbs.push(chunk);
            end = start;
        }
        let mut out = Self { neg, limbs };
        out.trim();
        Ok(out)
    }

    fn from_digits_radix(digits: &str, radix: u32, neg: bool, orig: &str) -> Result<Self, String> {
        let clean: String = digits.chars().filter(|&c| c != '_').collect();
        if clean.is_empty() {
            return Err(format!("invalid integer literal: {orig:?}"));
        }
        let mut acc = Self::zero();
        for ch in clean.chars() {
            let d = ch
                .to_digit(radix)
                .ok_or_else(|| format!("invalid base-{radix} digit {ch:?} in {orig:?}"))?;
            acc.mul_add_u32_in_place(radix as u64, d as u64);
        }
        acc.neg = neg && !acc.is_zero();
        Ok(acc)
    }

    pub fn to_dec_string(&self) -> String {
        if self.is_zero() {
            return "0".to_string();
        }
        let mut out = String::new();
        if self.neg {
            out.push('-');
        }
        let Some((last, rest)) = self.limbs.split_last() else {
            return "0".to_string();
        };
        out.push_str(&last.to_string());
        for &limb in rest.iter().rev() {
            out.push_str(&format!("{limb:09}"));
        }
        out
    }

    pub fn cmp_abs(&self, other: &Self) -> Ordering {
        if self.limbs.len() != other.limbs.len() {
            return self.limbs.len().cmp(&other.limbs.len());
        }
        for i in (0..self.limbs.len()).rev() {
            match self.limbs[i].cmp(&other.limbs[i]) {
                Ordering::Equal => continue,
                non_eq => return non_eq,
            }
        }
        Ordering::Equal
    }

    pub fn cmp_val(&self, other: &Self) -> Ordering {
        match (self.is_zero(), other.is_zero()) {
            (true, true) => return Ordering::Equal,
            (true, false) => {
                return if other.neg {
                    Ordering::Greater
                } else {
                    Ordering::Less
                };
            }
            (false, true) => {
                return if self.neg {
                    Ordering::Less
                } else {
                    Ordering::Greater
                };
            }
            (false, false) => {}
        }
        match (self.neg, other.neg) {
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            (false, false) => self.cmp_abs(other),
            (true, true) => other.cmp_abs(self),
        }
    }

    fn add_abs(a: &[u32], b: &[u32]) -> Vec<u32> {
        let len = a.len().max(b.len());
        let mut out = Vec::with_capacity(len + 1);
        let mut carry = 0u64;
        for i in 0..len {
            let av = *a.get(i).unwrap_or(&0) as u64;
            let bv = *b.get(i).unwrap_or(&0) as u64;
            let sum = av + bv + carry;
            if sum >= BASE {
                out.push((sum - BASE) as u32);
                carry = 1;
            } else {
                out.push(sum as u32);
                carry = 0;
            }
        }
        if carry > 0 {
            out.push(carry as u32);
        }
        out
    }

    // Assumes |a| >= |b|
    fn sub_abs(a: &[u32], b: &[u32]) -> Vec<u32> {
        let mut out = Vec::with_capacity(a.len());
        let mut borrow = 0i64;
        for i in 0..a.len() {
            let av = a[i] as i64 - borrow;
            let bv = *b.get(i).unwrap_or(&0) as i64;
            if av < bv {
                out.push((av + BASE as i64 - bv) as u32);
                borrow = 1;
            } else {
                out.push((av - bv) as u32);
                borrow = 0;
            }
        }
        while matches!(out.last(), Some(&0)) {
            out.pop();
        }
        out
    }

    pub fn add(&self, other: &Self) -> Self {
        if self.neg == other.neg {
            let mut res = Self {
                neg: self.neg,
                limbs: Self::add_abs(&self.limbs, &other.limbs),
            };
            res.trim();
            return res;
        }
        match self.cmp_abs(other) {
            Ordering::Equal => Self::zero(),
            Ordering::Greater => {
                let mut res = Self {
                    neg: self.neg,
                    limbs: Self::sub_abs(&self.limbs, &other.limbs),
                };
                res.trim();
                res
            }
            Ordering::Less => {
                let mut res = Self {
                    neg: other.neg,
                    limbs: Self::sub_abs(&other.limbs, &self.limbs),
                };
                res.trim();
                res
            }
        }
    }

    pub fn sub(&self, other: &Self) -> Self {
        self.add(&other.neg())
    }

    fn mul_abs_u32(a: &[u32], k: u32) -> Vec<u32> {
        if k == 0 || a.is_empty() {
            return Vec::new();
        }
        if k == 1 {
            return a.to_vec();
        }
        let mut out = Vec::with_capacity(a.len() + 1);
        let mut carry = 0u64;
        let ku = k as u64;
        for &av in a {
            let prod = (av as u64) * ku + carry;
            out.push((prod % BASE) as u32);
            carry = prod / BASE;
        }
        if carry > 0 {
            out.push(carry as u32);
        }
        out
    }

    pub fn mul(&self, other: &Self) -> Self {
        if self.is_zero() || other.is_zero() {
            return Self::zero();
        }
        let mut acc = vec![0u64; self.limbs.len() + other.limbs.len()];
        for (i, &av) in self.limbs.iter().enumerate() {
            let mut carry = 0u128;
            let a128 = av as u128;
            for (j, &bv) in other.limbs.iter().enumerate() {
                let idx = i + j;
                let prod = a128 * (bv as u128) + (acc[idx] as u128) + carry;
                acc[idx] = (prod % BASE as u128) as u64;
                carry = prod / BASE as u128;
            }
            if carry > 0 {
                acc[i + other.limbs.len()] += carry as u64;
            }
        }
        let mut limbs = Vec::with_capacity(acc.len() + 1);
        let mut carry = 0u64;
        for v in acc {
            let s = v + carry;
            limbs.push((s % BASE) as u32);
            carry = s / BASE;
        }
        while carry > 0 {
            limbs.push((carry % BASE) as u32);
            carry /= BASE;
        }
        let mut out = Self {
            neg: self.neg ^ other.neg,
            limbs,
        };
        out.trim();
        out
    }

    fn cmp_slice_abs(a: &[u32], b: &[u32]) -> Ordering {
        let alen = a.iter().rposition(|&x| x != 0).map_or(0, |p| p + 1);
        let blen = b.iter().rposition(|&x| x != 0).map_or(0, |p| p + 1);
        if alen != blen {
            return alen.cmp(&blen);
        }
        for i in (0..alen).rev() {
            match a[i].cmp(&b[i]) {
                Ordering::Equal => continue,
                ord => return ord,
            }
        }
        Ordering::Equal
    }

    pub fn div_rem(&self, divisor: &Self) -> Result<(Self, Self), String> {
        if divisor.is_zero() {
            return Err("division by zero".into());
        }
        if self.is_zero() {
            return Ok((Self::zero(), Self::zero()));
        }
        match self.cmp_abs(divisor) {
            Ordering::Less => return Ok((Self::zero(), self.clone())),
            Ordering::Equal => {
                let q = Self {
                    neg: self.neg ^ divisor.neg,
                    limbs: vec![1],
                };
                return Ok((q, Self::zero()));
            }
            Ordering::Greater => {}
        }

        if divisor.limbs.len() == 1 {
            let (mut q_mag, r_val) = self.div_rem_u64_mag(divisor.limbs[0] as u64);
            q_mag.neg = (self.neg ^ divisor.neg) && !q_mag.is_zero();
            let mut r = Self::from_u64(r_val);
            r.neg = self.neg && !r.is_zero();
            return Ok((q_mag, r));
        }

        // Base-10^9 long division with exact per-limb binary search in [0, BASE - 1].
        let mut rem_limbs: Vec<u32> = Vec::with_capacity(divisor.limbs.len() + 1);
        let mut q_limbs = vec![0u32; self.limbs.len()];
        for i in (0..self.limbs.len()).rev() {
            if !rem_limbs.is_empty() {
                rem_limbs.insert(0, self.limbs[i]);
            } else if self.limbs[i] != 0 {
                rem_limbs.push(self.limbs[i]);
            }
            if Self::cmp_slice_abs(&rem_limbs, &divisor.limbs) == Ordering::Less {
                q_limbs[i] = 0;
                continue;
            }
            let mut lo: u32 = 1;
            let mut hi: u32 = (BASE - 1) as u32;
            let mut best: u32 = 0;
            while lo <= hi {
                let mid = lo + (hi - lo) / 2;
                let cand = Self::mul_abs_u32(&divisor.limbs, mid);
                match Self::cmp_slice_abs(&cand, &rem_limbs) {
                    Ordering::Equal => {
                        best = mid;
                        break;
                    }
                    Ordering::Less => {
                        best = mid;
                        lo = mid + 1;
                    }
                    Ordering::Greater => {
                        hi = mid - 1;
                    }
                }
            }
            q_limbs[i] = best;
            if best > 0 {
                let sub = Self::mul_abs_u32(&divisor.limbs, best);
                rem_limbs = Self::sub_abs(&rem_limbs, &sub);
            }
        }

        let mut q = Self {
            neg: self.neg ^ divisor.neg,
            limbs: q_limbs,
        };
        q.trim();
        let mut r = Self {
            neg: self.neg,
            limbs: rem_limbs,
        };
        r.trim();
        Ok((q, r))
    }

    pub fn to_u64_checked(&self) -> Result<u64, String> {
        if self.neg {
            return Err("expected non-negative integer".into());
        }
        if self.is_zero() {
            return Ok(0);
        }
        let mut acc: u128 = 0;
        let base = BASE as u128;
        for &limb in self.limbs.iter().rev() {
            acc = acc
                .checked_mul(base)
                .and_then(|v| v.checked_add(limb as u128))
                .ok_or_else(|| "integer exceeds u64 range".to_string())?;
        }
        u64::try_from(acc).map_err(|_| "integer exceeds u64 range".to_string())
    }

    pub fn pow_u64(&self, mut exp: u64) -> Self {
        let mut base = self.clone();
        let mut acc = Self::one();
        while exp > 0 {
            if exp & 1 == 1 {
                acc = acc.mul(&base);
            }
            exp >>= 1;
            if exp > 0 {
                base = base.mul(&base);
            }
        }
        acc
    }

    pub fn mul_pow10(&self, k: usize) -> Self {
        if self.is_zero() || k == 0 {
            return self.clone();
        }
        let whole_limbs = k / BASE_DIGITS;
        let rem_digits = k % BASE_DIGITS;
        let factor = 10u32.pow(rem_digits as u32);
        let scaled = Self::mul_abs_u32(&self.limbs, factor);
        let mut limbs = vec![0u32; whole_limbs];
        limbs.extend_from_slice(&scaled);
        let mut out = Self {
            neg: self.neg,
            limbs,
        };
        out.trim();
        out
    }

    pub fn mod_pow(&self, exp: &Self, modulus: &Self) -> Result<Self, String> {
        if modulus.is_zero() {
            return Err("mod_pow modulus cannot be zero".into());
        }
        if exp.neg {
            return Err("mod_pow exponent must be non-negative".into());
        }
        let m_abs = modulus.abs();
        if m_abs.limbs == [1] {
            return Ok(Self::zero());
        }
        let (_, mut r0) = self.div_rem(&m_abs)?;
        if r0.neg {
            r0 = r0.add(&m_abs);
        }
        let mut base = r0;
        let mut acc = Self::one();
        // Extract 30 bits at a time by dividing by 2^30 (since BASE = 10^9 < 2^30).
        let mut e = exp.abs();
        let chunk_mod = 1u64 << 30;
        while !e.is_zero() {
            let (next_e, low_bits) = e.div_rem_u64_mag(chunk_mod);
            let nbits = if next_e.is_zero() {
                (64 - low_bits.leading_zeros()).max(1)
            } else {
                30
            };
            for bit in 0..nbits {
                if (low_bits >> bit) & 1 == 1 {
                    let prod = acc.mul(&base);
                    acc = prod.div_rem(&m_abs)?.1;
                }
                if !(next_e.is_zero() && bit + 1 == nbits) {
                    let sq = base.mul(&base);
                    base = sq.div_rem(&m_abs)?.1;
                }
            }
            e = next_e;
        }
        Ok(acc)
    }

    pub fn gcd(&self, other: &Self) -> Self {
        let mut a = self.abs();
        let mut b = other.abs();
        while !b.is_zero() {
            let (_, r) = match a.div_rem(&b) {
                Ok(pair) => pair,
                Err(_) => break,
            };
            a = b;
            b = r;
        }
        a
    }

    pub fn bit_length(&self) -> usize {
        if self.is_zero() {
            return 0;
        }
        let mut cur = self.abs();
        let mut bits = 0usize;
        let chunk_mod = 1u64 << 30;
        loop {
            let (next, low) = cur.div_rem_u64_mag(chunk_mod);
            if next.is_zero() {
                bits += (64 - low.leading_zeros()) as usize;
                break;
            }
            bits += 30;
            cur = next;
        }
        bits
    }
}

pub fn bigint_norm(s: &str) -> Result<String, String> {
    Ok(BigInt::from_str_radix_auto(s)?.to_dec_string())
}

pub fn bigint_add(a: &str, b: &str) -> Result<String, String> {
    let ba = BigInt::from_str_radix_auto(a)?;
    let bb = BigInt::from_str_radix_auto(b)?;
    Ok(ba.add(&bb).to_dec_string())
}

pub fn bigint_sub(a: &str, b: &str) -> Result<String, String> {
    let ba = BigInt::from_str_radix_auto(a)?;
    let bb = BigInt::from_str_radix_auto(b)?;
    Ok(ba.sub(&bb).to_dec_string())
}

pub fn bigint_mul(a: &str, b: &str) -> Result<String, String> {
    let ba = BigInt::from_str_radix_auto(a)?;
    let bb = BigInt::from_str_radix_auto(b)?;
    Ok(ba.mul(&bb).to_dec_string())
}

pub fn bigint_div(a: &str, b: &str) -> Result<String, String> {
    let ba = BigInt::from_str_radix_auto(a)?;
    let bb = BigInt::from_str_radix_auto(b)?;
    Ok(ba.div_rem(&bb)?.0.to_dec_string())
}

pub fn bigint_rem(a: &str, b: &str) -> Result<String, String> {
    let ba = BigInt::from_str_radix_auto(a)?;
    let bb = BigInt::from_str_radix_auto(b)?;
    Ok(ba.div_rem(&bb)?.1.to_dec_string())
}

pub fn bigint_divmod(a: &str, b: &str) -> Result<(String, String), String> {
    let ba = BigInt::from_str_radix_auto(a)?;
    let bb = BigInt::from_str_radix_auto(b)?;
    let (q, r) = ba.div_rem(&bb)?;
    Ok((q.to_dec_string(), r.to_dec_string()))
}

pub fn bigint_pow(a: &str, exp: &str) -> Result<String, String> {
    let ba = BigInt::from_str_radix_auto(a)?;
    let be = BigInt::from_str_radix_auto(exp)?;
    let eu = be.to_u64_checked()?;
    if eu > 1_000_000 {
        return Err(format!("big_pow exponent {eu} exceeds safety cap 1000000"));
    }
    Ok(ba.pow_u64(eu).to_dec_string())
}

pub fn bigint_powmod(a: &str, exp: &str, m: &str) -> Result<String, String> {
    let ba = BigInt::from_str_radix_auto(a)?;
    let be = BigInt::from_str_radix_auto(exp)?;
    let bm = BigInt::from_str_radix_auto(m)?;
    Ok(ba.mod_pow(&be, &bm)?.to_dec_string())
}

pub fn bigint_gcd(a: &str, b: &str) -> Result<String, String> {
    let ba = BigInt::from_str_radix_auto(a)?;
    let bb = BigInt::from_str_radix_auto(b)?;
    Ok(ba.gcd(&bb).to_dec_string())
}

pub fn bigint_cmp(a: &str, b: &str) -> Result<i32, String> {
    let ba = BigInt::from_str_radix_auto(a)?;
    let bb = BigInt::from_str_radix_auto(b)?;
    Ok(match ba.cmp_val(&bb) {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    })
}

pub fn bigint_bit_length(a: &str) -> Result<usize, String> {
    Ok(BigInt::from_str_radix_auto(a)?.bit_length())
}

#[derive(Clone, Debug)]
pub struct BigDecimal {
    pub coeff: BigInt,
    pub scale: i64, // value = coeff * 10^(-scale)
}

impl BigDecimal {
    pub fn parse(s: &str) -> Result<Self, String> {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return Err("empty decimal string".into());
        }
        let (neg, mut body) = if let Some(r) = trimmed.strip_prefix('-') {
            (true, r.trim())
        } else if let Some(r) = trimmed.strip_prefix('+') {
            (false, r.trim())
        } else {
            (false, trimmed)
        };
        let mut exp_shift: i64 = 0;
        if let Some(pos) = body.find(['e', 'E']) {
            let exp_part: String = body[pos + 1..].chars().filter(|&c| c != '_').collect();
            exp_shift = exp_part
                .parse::<i64>()
                .map_err(|_| format!("invalid exponent in decimal {s:?}"))?;
            body = &body[..pos];
        }
        let clean: String = body.chars().filter(|&c| c != '_').collect();
        let (int_part, frac_part) = match clean.split_once('.') {
            Some((a, b)) => {
                if b.contains('.') {
                    return Err(format!("multiple decimal points in {s:?}"));
                }
                (a, b)
            }
            None => (clean.as_str(), ""),
        };
        if int_part.is_empty() && frac_part.is_empty() {
            return Err(format!("invalid decimal literal: {s:?}"));
        }
        if !int_part.bytes().all(|b| b.is_ascii_digit())
            || !frac_part.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(format!("invalid decimal digits in {s:?}"));
        }
        let digits = format!("{int_part}{frac_part}");
        let mut coeff = BigInt::from_str_radix_auto(&digits)?;
        if neg && !coeff.is_zero() {
            coeff.neg = true;
        }
        let scale = (frac_part.len() as i64) - exp_shift;
        Ok(Self { coeff, scale }.normalized())
    }

    pub fn normalized(mut self) -> Self {
        if self.coeff.is_zero() {
            return Self {
                coeff: BigInt::zero(),
                scale: 0,
            };
        }
        if self.scale < 0 {
            self.coeff = self.coeff.mul_pow10((-self.scale) as usize);
            self.scale = 0;
            return self;
        }
        let mut s = self.coeff.abs().to_dec_string();
        let mut trimmed_zeros = 0usize;
        while self.scale > 0 && s.ends_with('0') && (trimmed_zeros as i64) < self.scale {
            s.pop();
            trimmed_zeros += 1;
        }
        if trimmed_zeros > 0 {
            let neg = self.coeff.neg;
            let mut next = BigInt::from_str_radix_auto(&s).unwrap_or_else(|_| BigInt::zero());
            next.neg = neg && !next.is_zero();
            self.coeff = next;
            self.scale -= trimmed_zeros as i64;
        }
        self
    }

    pub fn format_canonical(&self) -> String {
        let norm = self.clone().normalized();
        if norm.coeff.is_zero() {
            return "0".to_string();
        }
        let neg = norm.coeff.neg;
        let digits = norm.coeff.abs().to_dec_string();
        let sign = if neg { "-" } else { "" };
        if norm.scale <= 0 {
            let zeros = "0".repeat((-norm.scale) as usize);
            return format!("{sign}{digits}{zeros}");
        }
        let scale = norm.scale as usize;
        if digits.len() <= scale {
            let pad = "0".repeat(scale - digits.len());
            return format!("{sign}0.{pad}{digits}");
        }
        let split = digits.len() - scale;
        format!("{sign}{}.{}", &digits[..split], &digits[split..])
    }

    pub fn format_fixed(&self, places: usize) -> String {
        let rounded = self.round_places(places as i64);
        if places == 0 {
            return rounded.coeff.to_dec_string();
        }
        let neg = rounded.coeff.neg;
        let mut digits = rounded.coeff.abs().to_dec_string();
        let cur_scale = rounded.scale.max(0) as usize;
        if cur_scale < places {
            digits.push_str(&"0".repeat(places - cur_scale));
        }
        let sign = if neg { "-" } else { "" };
        if digits.len() <= places {
            let pad = "0".repeat(places - digits.len());
            format!("{sign}0.{pad}{digits}")
        } else {
            let split = digits.len() - places;
            format!("{sign}{}.{}", &digits[..split], &digits[split..])
        }
    }

    fn align(a: &Self, b: &Self) -> (BigInt, BigInt, i64) {
        let target = a.scale.max(b.scale);
        let ca = if a.scale < target {
            a.coeff.mul_pow10((target - a.scale) as usize)
        } else {
            a.coeff.clone()
        };
        let cb = if b.scale < target {
            b.coeff.mul_pow10((target - b.scale) as usize)
        } else {
            b.coeff.clone()
        };
        (ca, cb, target)
    }

    pub fn add(&self, other: &Self) -> Self {
        let (ca, cb, scale) = Self::align(self, other);
        Self {
            coeff: ca.add(&cb),
            scale,
        }
        .normalized()
    }

    pub fn sub(&self, other: &Self) -> Self {
        let (ca, cb, scale) = Self::align(self, other);
        Self {
            coeff: ca.sub(&cb),
            scale,
        }
        .normalized()
    }

    pub fn mul(&self, other: &Self) -> Self {
        Self {
            coeff: self.coeff.mul(&other.coeff),
            scale: self.scale + other.scale,
        }
        .normalized()
    }

    pub fn div_prec(&self, other: &Self, prec: usize) -> Result<Self, String> {
        if other.coeff.is_zero() {
            return Err("decimal division by zero".into());
        }
        if self.coeff.is_zero() {
            return Ok(Self {
                coeff: BigInt::zero(),
                scale: 0,
            });
        }
        let neg = self.coeff.neg ^ other.coeff.neg;
        let a_mag = self.coeff.abs();
        let b_mag = other.coeff.abs();
        // Quotient = (a_mag / b_mag) * 10^(b.scale - a.scale). Compute to fractional scale `prec`.
        let extra_shift = (prec as i64) + other.scale - self.scale;
        let (mut q, r) = if extra_shift >= 0 {
            let scaled_a = a_mag.mul_pow10(extra_shift as usize);
            scaled_a.div_rem(&b_mag)?
        } else {
            let scaled_b = b_mag.mul_pow10((-extra_shift) as usize);
            a_mag.div_rem(&scaled_b)?
        };
        let twice_r = r.add(&r);
        let cmp_b = if extra_shift >= 0 {
            twice_r.cmp_abs(&b_mag)
        } else {
            let scaled_b = b_mag.mul_pow10((-extra_shift) as usize);
            twice_r.cmp_abs(&scaled_b)
        };
        if cmp_b != Ordering::Less {
            q = q.add(&BigInt::one());
        }
        if neg && !q.is_zero() {
            q.neg = true;
        }
        Ok(Self {
            coeff: q,
            scale: prec as i64,
        }
        .normalized())
    }

    pub fn round_places(&self, places: i64) -> Self {
        if self.scale <= places {
            return Self {
                coeff: self.coeff.mul_pow10((places - self.scale) as usize),
                scale: places,
            };
        }
        let drop_digits = (self.scale - places) as usize;
        let neg = self.coeff.neg;
        let mag = self.coeff.abs();
        let divisor = BigInt::one().mul_pow10(drop_digits);
        let (mut q, r) = mag
            .div_rem(&divisor)
            .unwrap_or((BigInt::zero(), BigInt::zero()));
        if r.add(&r).cmp_abs(&divisor) != Ordering::Less {
            q = q.add(&BigInt::one());
        }
        if neg && !q.is_zero() {
            q.neg = true;
        }
        Self {
            coeff: q,
            scale: places,
        }
    }

    pub fn sqrt_prec(&self, prec: usize) -> Result<Self, String> {
        if self.coeff.neg {
            return Err("decimal square root of negative number".into());
        }
        if self.coeff.is_zero() {
            return Ok(Self {
                coeff: BigInt::zero(),
                scale: 0,
            });
        }
        let guard = prec + 6;
        let shift = (2 * guard as i64) - self.scale;
        let scaled = if shift >= 0 {
            self.coeff.mul_pow10(shift as usize)
        } else {
            let div = BigInt::one().mul_pow10((-shift) as usize);
            self.coeff.div_rem(&div)?.0
        };
        // Newton-Raphson integer floor sqrt of `scaled` starting from a strict upper bound.
        let digits = scaled.to_dec_string().len();
        let mut x = BigInt::one().mul_pow10(digits.div_ceil(2));
        let two = BigInt::from_u64(2);
        loop {
            let q = scaled.div_rem(&x)?.0;
            let next = x.add(&q).div_rem(&two)?.0;
            if next.cmp_val(&x) != Ordering::Less {
                break;
            }
            x = next;
        }
        let raw = Self {
            coeff: x,
            scale: guard as i64,
        };
        Ok(raw.round_places(prec as i64).normalized())
    }
}

pub fn dec_norm(s: &str) -> Result<String, String> {
    Ok(BigDecimal::parse(s)?.format_canonical())
}

pub fn dec_add(a: &str, b: &str) -> Result<String, String> {
    Ok(BigDecimal::parse(a)?
        .add(&BigDecimal::parse(b)?)
        .format_canonical())
}

pub fn dec_sub(a: &str, b: &str) -> Result<String, String> {
    Ok(BigDecimal::parse(a)?
        .sub(&BigDecimal::parse(b)?)
        .format_canonical())
}

pub fn dec_mul(a: &str, b: &str) -> Result<String, String> {
    Ok(BigDecimal::parse(a)?
        .mul(&BigDecimal::parse(b)?)
        .format_canonical())
}

pub fn dec_div(a: &str, b: &str, prec: usize) -> Result<String, String> {
    Ok(BigDecimal::parse(a)?
        .div_prec(&BigDecimal::parse(b)?, prec)?
        .format_canonical())
}

pub fn dec_round(a: &str, places: i64) -> Result<String, String> {
    Ok(BigDecimal::parse(a)?
        .round_places(places)
        .format_canonical())
}

pub fn dec_quantize(a: &str, places: usize) -> Result<String, String> {
    Ok(BigDecimal::parse(a)?.format_fixed(places))
}

pub fn dec_sqrt(a: &str, prec: usize) -> Result<String, String> {
    Ok(BigDecimal::parse(a)?.sqrt_prec(prec)?.format_canonical())
}

pub fn dec_cmp(a: &str, b: &str) -> Result<i32, String> {
    let da = BigDecimal::parse(a)?;
    let db = BigDecimal::parse(b)?;
    let (ca, cb, _) = BigDecimal::align(&da, &db);
    Ok(match ca.cmp_val(&cb) {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    })
}

pub fn dec_abs(a: &str) -> Result<String, String> {
    let mut d = BigDecimal::parse(a)?;
    d.coeff.neg = false;
    Ok(d.format_canonical())
}

pub fn dec_neg(a: &str) -> Result<String, String> {
    let mut d = BigDecimal::parse(a)?;
    d.coeff = d.coeff.neg();
    Ok(d.format_canonical())
}

#[inline]
pub fn c_add(a: (f64, f64), b: (f64, f64)) -> (f64, f64) {
    (a.0 + b.0, a.1 + b.1)
}

#[inline]
pub fn c_sub(a: (f64, f64), b: (f64, f64)) -> (f64, f64) {
    (a.0 - b.0, a.1 - b.1)
}

#[inline]
pub fn c_mul(a: (f64, f64), b: (f64, f64)) -> (f64, f64) {
    (a.0 * b.0 - a.1 * b.1, a.0 * b.1 + a.1 * b.0)
}

pub fn c_div(a: (f64, f64), b: (f64, f64)) -> Result<(f64, f64), String> {
    let denom = b.0 * b.0 + b.1 * b.1;
    if denom == 0.0 {
        return Err("complex division by zero".into());
    }
    Ok((
        (a.0 * b.0 + a.1 * b.1) / denom,
        (a.1 * b.0 - a.0 * b.1) / denom,
    ))
}

#[inline]
pub fn c_abs(a: (f64, f64)) -> f64 {
    a.0.hypot(a.1)
}

#[inline]
pub fn c_arg(a: (f64, f64)) -> f64 {
    a.1.atan2(a.0)
}

#[inline]
pub fn c_exp(a: (f64, f64)) -> (f64, f64) {
    let r = a.0.exp();
    (r * a.1.cos(), r * a.1.sin())
}

fn fft_radix2_in_place(buf: &mut [(f64, f64)], inverse: bool) {
    let n = buf.len();
    debug_assert!(n.is_power_of_two());
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j ^= bit;
        if i < j {
            buf.swap(i, j);
        }
    }
    let sign = if inverse { 1.0 } else { -1.0 };
    let mut len = 2usize;
    while len <= n {
        let ang = sign * 2.0 * PI / (len as f64);
        let wlen = (ang.cos(), ang.sin());
        let half = len >> 1;
        for start in (0..n).step_by(len) {
            let mut w = (1.0, 0.0);
            for k in 0..half {
                let u = buf[start + k];
                let v = c_mul(buf[start + k + half], w);
                buf[start + k] = c_add(u, v);
                buf[start + k + half] = c_sub(u, v);
                w = c_mul(w, wlen);
            }
        }
        len <<= 1;
    }
    if inverse {
        let inv_n = 1.0 / (n as f64);
        for x in buf.iter_mut() {
            x.0 *= inv_n;
            x.1 *= inv_n;
        }
    }
}

fn fft_core(input: &[(f64, f64)], inverse: bool) -> Vec<(f64, f64)> {
    let n = input.len();
    if n <= 1 {
        return input.to_vec();
    }
    if n.is_power_of_two() {
        let mut buf = input.to_vec();
        fft_radix2_in_place(&mut buf, inverse);
        return buf;
    }
    // Bluestein's algorithm for arbitrary N in O(N log N).
    let m = (2 * n - 1).next_power_of_two();
    let sign = if inverse { 1.0 } else { -1.0 };
    let mut a = vec![(0.0, 0.0); m];
    let mut b = vec![(0.0, 0.0); m];
    let mut chirp = Vec::with_capacity(n);
    for k in 0..n {
        let kk = (k * k) % (2 * n);
        let angle = sign * PI * (kk as f64) / (n as f64);
        let w = (angle.cos(), angle.sin());
        chirp.push(w);
        a[k] = c_mul(input[k], w);
        b[k] = (w.0, -w.1);
        if k > 0 {
            b[m - k] = (w.0, -w.1);
        }
    }
    fft_radix2_in_place(&mut a, false);
    fft_radix2_in_place(&mut b, false);
    for i in 0..m {
        a[i] = c_mul(a[i], b[i]);
    }
    fft_radix2_in_place(&mut a, true);
    let scale = if inverse { 1.0 / (n as f64) } else { 1.0 };
    (0..n)
        .map(|k| {
            let v = c_mul(a[k], chirp[k]);
            (v.0 * scale, v.1 * scale)
        })
        .collect()
}

pub fn fft_compute(input: &[(f64, f64)]) -> Vec<(f64, f64)> {
    fft_core(input, false)
}

pub fn ifft_compute(input: &[(f64, f64)]) -> Vec<(f64, f64)> {
    fft_core(input, true)
}

#[derive(Clone, Debug)]
pub enum BigArg {
    Str(String),
    Num(f64),
    Complex(f64, f64),
    List(Vec<BigArg>),
    Other,
}

#[derive(Clone, Debug)]
pub enum BigOut {
    Str(String),
    Num(f64),
    StrPair(String, String),
    Complex(f64, f64),
    ComplexList(Vec<(f64, f64)>),
}

pub fn is_bigint_builtin(name: &str) -> bool {
    matches!(
        name,
        "bigint"
            | "big_add"
            | "big_sub"
            | "big_mul"
            | "big_div"
            | "big_rem"
            | "big_divmod"
            | "big_pow"
            | "big_powmod"
            | "big_gcd"
            | "big_cmp"
            | "big_bits"
            | "dec"
            | "dec_add"
            | "dec_sub"
            | "dec_mul"
            | "dec_div"
            | "dec_sqrt"
            | "dec_round"
            | "dec_quantize"
            | "dec_cmp"
            | "dec_abs"
            | "dec_neg"
            | "complex"
            | "cadd"
            | "csub"
            | "cmul"
            | "cdiv"
            | "cabs"
            | "carg"
            | "cexp"
            | "fft"
            | "ifft"
    )
}

fn arg_to_big_str(a: &BigArg) -> Result<String, String> {
    match a {
        BigArg::Str(s) => Ok(s.clone()),
        BigArg::Num(n) if n.is_finite() && n.fract() == 0.0 => Ok(format!("{n:.0}")),
        BigArg::Num(_) => Err("BigInt number must be finite integer".into()),
        _ => Err("expected String or integer Number".into()),
    }
}

fn arg_to_dec_str(a: &BigArg) -> Result<String, String> {
    match a {
        BigArg::Str(s) => Ok(s.clone()),
        BigArg::Num(n) if n.is_finite() => {
            if n.fract() == 0.0 && n.abs() < 9e15 {
                Ok(format!("{}", *n as i64))
            } else {
                Ok(format!("{n}"))
            }
        }
        BigArg::Num(_) => Err("Decimal number must be finite".into()),
        _ => Err("expected String or Number".into()),
    }
}

fn arg_to_complex(a: &BigArg) -> Result<(f64, f64), String> {
    match a {
        BigArg::Num(r) => Ok((*r, 0.0)),
        BigArg::Complex(r, i) => Ok((*r, *i)),
        BigArg::List(xs) if xs.len() == 2 => match (&xs[0], &xs[1]) {
            (BigArg::Num(r), BigArg::Num(i)) => Ok((*r, *i)),
            _ => Err("complex elements must be Numbers".into()),
        },
        _ => Err("expected Complex [re, im] or Number".into()),
    }
}

fn arg_to_usize(a: &BigArg, what: &str) -> Result<usize, String> {
    match a {
        BigArg::Num(n) if n.is_finite() && *n >= 0.0 => Ok(*n as usize),
        BigArg::Num(_) => Err(format!("{what} must be non-negative")),
        _ => Err(format!("{what} must be Number")),
    }
}

pub fn eval_bigint_builtin(name: &str, a: &[BigArg]) -> Result<Option<BigOut>, String> {
    let out = match name {
        "bigint" => {
            if a.len() != 1 {
                return Err("bigint expects 1 argument".into());
            }
            BigOut::Str(bigint_norm(&arg_to_big_str(&a[0])?)?)
        }
        "big_add" | "big_sub" | "big_mul" | "big_div" | "big_rem" | "big_pow" | "big_gcd" => {
            if a.len() != 2 {
                return Err(format!("{name} expects 2 arguments"));
            }
            let sa = arg_to_big_str(&a[0])?;
            let sb = arg_to_big_str(&a[1])?;
            let s = match name {
                "big_add" => bigint_add(&sa, &sb)?,
                "big_sub" => bigint_sub(&sa, &sb)?,
                "big_mul" => bigint_mul(&sa, &sb)?,
                "big_div" => bigint_div(&sa, &sb)?,
                "big_rem" => bigint_rem(&sa, &sb)?,
                "big_pow" => bigint_pow(&sa, &sb)?,
                _ => bigint_gcd(&sa, &sb)?,
            };
            BigOut::Str(s)
        }
        "big_divmod" => {
            if a.len() != 2 {
                return Err("big_divmod expects 2 arguments".into());
            }
            let (q, r) = bigint_divmod(&arg_to_big_str(&a[0])?, &arg_to_big_str(&a[1])?)?;
            BigOut::StrPair(q, r)
        }
        "big_powmod" => {
            if a.len() != 3 {
                return Err("big_powmod expects (base, exp, mod)".into());
            }
            BigOut::Str(bigint_powmod(
                &arg_to_big_str(&a[0])?,
                &arg_to_big_str(&a[1])?,
                &arg_to_big_str(&a[2])?,
            )?)
        }
        "big_cmp" => {
            if a.len() != 2 {
                return Err("big_cmp expects 2 arguments".into());
            }
            BigOut::Num(bigint_cmp(&arg_to_big_str(&a[0])?, &arg_to_big_str(&a[1])?)? as f64)
        }
        "big_bits" => {
            if a.len() != 1 {
                return Err("big_bits expects 1 argument".into());
            }
            BigOut::Num(bigint_bit_length(&arg_to_big_str(&a[0])?)? as f64)
        }
        "dec" => {
            if a.len() != 1 {
                return Err("dec expects 1 argument".into());
            }
            BigOut::Str(dec_norm(&arg_to_dec_str(&a[0])?)?)
        }
        "dec_add" | "dec_sub" | "dec_mul" => {
            if a.len() != 2 {
                return Err(format!("{name} expects 2 arguments"));
            }
            let sa = arg_to_dec_str(&a[0])?;
            let sb = arg_to_dec_str(&a[1])?;
            let s = match name {
                "dec_add" => dec_add(&sa, &sb)?,
                "dec_sub" => dec_sub(&sa, &sb)?,
                _ => dec_mul(&sa, &sb)?,
            };
            BigOut::Str(s)
        }
        "dec_div" => {
            if a.len() < 2 || a.len() > 3 {
                return Err("dec_div expects (a, b[, prec])".into());
            }
            let prec = match a.get(2) {
                Some(p) => arg_to_usize(p, "prec")?,
                None => 28,
            };
            BigOut::Str(dec_div(
                &arg_to_dec_str(&a[0])?,
                &arg_to_dec_str(&a[1])?,
                prec,
            )?)
        }
        "dec_sqrt" => {
            if a.is_empty() || a.len() > 2 {
                return Err("dec_sqrt expects (a[, prec])".into());
            }
            let prec = match a.get(1) {
                Some(p) => arg_to_usize(p, "prec")?,
                None => 28,
            };
            BigOut::Str(dec_sqrt(&arg_to_dec_str(&a[0])?, prec)?)
        }
        "dec_round" => {
            if a.is_empty() || a.len() > 2 {
                return Err("dec_round expects (a[, places])".into());
            }
            let places = match a.get(1) {
                None => 0,
                Some(BigArg::Num(p)) if p.is_finite() => *p as i64,
                _ => return Err("places must be Number".into()),
            };
            BigOut::Str(dec_round(&arg_to_dec_str(&a[0])?, places)?)
        }
        "dec_quantize" => {
            if a.len() != 2 {
                return Err("dec_quantize expects (a, places)".into());
            }
            let places = arg_to_usize(&a[1], "places")?;
            BigOut::Str(dec_quantize(&arg_to_dec_str(&a[0])?, places)?)
        }
        "dec_cmp" => {
            if a.len() != 2 {
                return Err("dec_cmp expects 2 arguments".into());
            }
            BigOut::Num(dec_cmp(&arg_to_dec_str(&a[0])?, &arg_to_dec_str(&a[1])?)? as f64)
        }
        "dec_abs" | "dec_neg" => {
            if a.len() != 1 {
                return Err(format!("{name} expects 1 argument"));
            }
            let sa = arg_to_dec_str(&a[0])?;
            BigOut::Str(if name == "dec_abs" {
                dec_abs(&sa)?
            } else {
                dec_neg(&sa)?
            })
        }
        "complex" => match a {
            [x] => {
                let z = arg_to_complex(x)?;
                BigOut::Complex(z.0, z.1)
            }
            [BigArg::Num(re), BigArg::Num(im)] => BigOut::Complex(*re, *im),
            _ => return Err("complex expects (re[, im])".into()),
        },
        "cadd" | "csub" | "cmul" | "cdiv" => {
            if a.len() != 2 {
                return Err(format!("{name} expects 2 complex numbers"));
            }
            let za = arg_to_complex(&a[0])?;
            let zb = arg_to_complex(&a[1])?;
            let z = match name {
                "cadd" => c_add(za, zb),
                "csub" => c_sub(za, zb),
                "cmul" => c_mul(za, zb),
                _ => c_div(za, zb)?,
            };
            BigOut::Complex(z.0, z.1)
        }
        "cabs" | "carg" | "cexp" => {
            if a.len() != 1 {
                return Err(format!("{name} expects 1 complex number"));
            }
            let z = arg_to_complex(&a[0])?;
            match name {
                "cabs" => BigOut::Num(c_abs(z)),
                "carg" => BigOut::Num(c_arg(z)),
                _ => {
                    let e = c_exp(z);
                    BigOut::Complex(e.0, e.1)
                }
            }
        }
        "fft" | "ifft" => {
            if a.len() != 1 {
                return Err(format!("{name} expects 1 List argument"));
            }
            let BigArg::List(items) = &a[0] else {
                return Err("expected List of Numbers or [re, im] pairs".into());
            };
            let xs: Vec<(f64, f64)> = items.iter().map(arg_to_complex).collect::<Result<_, _>>()?;
            BigOut::ComplexList(if name == "fft" {
                fft_compute(&xs)
            } else {
                ifft_compute(&xs)
            })
        }
        _ => return Ok(None),
    };
    Ok(Some(out))
}

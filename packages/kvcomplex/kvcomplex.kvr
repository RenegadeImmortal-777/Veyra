fn make(re: Number, im: Number) -> List {
    return complex(re, im);
}

fn from_polar(r: Number, theta: Number) -> List {
    return complex(r * cos(theta), r * sin(theta));
}

fn add(a: List, b: List) -> List {
    return cadd(a, b);
}

fn sub(a: List, b: List) -> List {
    return csub(a, b);
}

fn mul(a: List, b: List) -> List {
    return cmul(a, b);
}

fn div(a: List, b: List) -> List {
    return cdiv(a, b);
}

fn conj(z: List) -> List {
    return complex(z[0], 0.0 - z[1]);
}

fn abs_val(z: List) -> Number {
    return cabs(z);
}

fn phase(z: List) -> Number {
    return carg(z);
}

fn polar(z: List) -> List {
    return [cabs(z), carg(z)];
}

fn exp_c(z: List) -> List {
    return cexp(z);
}

fn pow_int(z: List, n: Number) -> List {
    let exp_n = int(n);
    if exp_n == 0 {
        return complex(1.0, 0.0);
    }
    let base = z;
    if exp_n < 0 {
        base = cdiv(complex(1.0, 0.0), z);
        exp_n = 0 - exp_n;
    }
    let res = complex(1.0, 0.0);
    let i = 0;
    while i < exp_n {
        res = cmul(res, base);
        i = i + 1;
    }
    return res;
}

fn fft_list(xs: List) -> List {
    return fft(xs);
}

fn ifft_list(xs: List) -> List {
    return ifft(xs);
}

class Complex {
    fn init(self, re, im) {
        let c = complex(re, im);
        self.re = c[0];
        self.im = c[1];
    }

    fn to_pair(self) {
        return [self.re, self.im];
    }

    fn conj(self) {
        return Complex(self.re, 0.0 - self.im);
    }

    fn abs(self) {
        return cabs([self.re, self.im]);
    }

    fn phase(self) {
        return carg([self.re, self.im]);
    }

    fn __add__(self, other) {
        let s = cadd([self.re, self.im], [other.re, other.im]);
        return Complex(s[0], s[1]);
    }

    fn __sub__(self, other) {
        let s = csub([self.re, self.im], [other.re, other.im]);
        return Complex(s[0], s[1]);
    }

    fn __mul__(self, other) {
        let s = cmul([self.re, self.im], [other.re, other.im]);
        return Complex(s[0], s[1]);
    }

    fn __truediv__(self, other) {
        let s = cdiv([self.re, self.im], [other.re, other.im]);
        return Complex(s[0], s[1]);
    }

    fn __str__(self) {
        if self.im >= 0 {
            return str(self.re) + "+" + str(self.im) + "j";
        }
        return str(self.re) + str(self.im) + "j";
    }
}

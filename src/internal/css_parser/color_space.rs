//! Port of upstream `internal/css_parser/css_color_spaces.go`.

use crate::internal::helpers::{F64, max2, max3, min2, min3};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ColorSpace {
    A98Rgb,
    DisplayP3,
    Hsl,
    Hwb,
    Lab,
    Lch,
    Oklab,
    Oklch,
    ProphotoRgb,
    Rec2020,
    Srgb,
    SrgbLinear,
    Xyz,
    XyzD50,
    XyzD65,
}

impl ColorSpace {
    pub(super) fn is_polar(self) -> bool {
        matches!(self, Self::Hsl | Self::Hwb | Self::Lch | Self::Oklch)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum HueMethod {
    #[default]
    Shorter,
    Longer,
    Increasing,
    Decreasing,
}

fn map3(a: F64, b: F64, c: F64, f: impl Fn(F64) -> F64) -> (F64, F64, F64) {
    (f(a), f(b), f(c))
}

fn apply3(
    value: (F64, F64, F64),
    function: fn(F64, F64, F64) -> (F64, F64, F64),
) -> (F64, F64, F64) {
    function(value.0, value.1, value.2)
}

pub(super) fn lin_srgb(r: F64, g: F64, b: F64) -> (F64, F64, F64) {
    map3(r, g, b, |value| {
        let abs = value.abs();
        if abs.value() < 0.04045 {
            value.div_const(12.92)
        } else {
            abs.add_const(0.055)
                .div_const(1.055)
                .pow_const(2.4)
                .with_sign_from(value)
        }
    })
}

pub(super) fn gam_srgb(r: F64, g: F64, b: F64) -> (F64, F64, F64) {
    map3(r, g, b, |value| {
        let abs = value.abs();
        if abs.value() > 0.003_130_8 {
            abs.pow_const(1.0 / 2.4)
                .mul_const(1.055)
                .sub_const(0.055)
                .with_sign_from(value)
        } else {
            value.mul_const(12.92)
        }
    })
}

fn multiply(matrix: [f64; 9], a: F64, b: F64, c: F64) -> (F64, F64, F64) {
    (
        a.mul_const(matrix[0])
            .add(b.mul_const(matrix[1]))
            .add(c.mul_const(matrix[2])),
        a.mul_const(matrix[3])
            .add(b.mul_const(matrix[4]))
            .add(c.mul_const(matrix[5])),
        a.mul_const(matrix[6])
            .add(b.mul_const(matrix[7]))
            .add(c.mul_const(matrix[8])),
    )
}

pub(super) fn lin_srgb_to_xyz(r: F64, g: F64, b: F64) -> (F64, F64, F64) {
    multiply(
        [
            506752.0 / 1228815.0,
            87881.0 / 245763.0,
            12673.0 / 70218.0,
            87098.0 / 409605.0,
            175762.0 / 245763.0,
            12673.0 / 175545.0,
            7918.0 / 409605.0,
            87881.0 / 737289.0,
            1001167.0 / 1053270.0,
        ],
        r,
        g,
        b,
    )
}

pub(super) fn xyz_to_lin_srgb(x: F64, y: F64, z: F64) -> (F64, F64, F64) {
    multiply(
        [
            12831.0 / 3959.0,
            -329.0 / 214.0,
            -1974.0 / 3959.0,
            -851781.0 / 878810.0,
            1648619.0 / 878810.0,
            36519.0 / 878810.0,
            705.0 / 12673.0,
            -2585.0 / 12673.0,
            705.0 / 667.0,
        ],
        x,
        y,
        z,
    )
}

fn lin_p3_to_xyz(r: F64, g: F64, b: F64) -> (F64, F64, F64) {
    multiply(
        [
            608311.0 / 1250200.0,
            189793.0 / 714400.0,
            198249.0 / 1000160.0,
            35783.0 / 156275.0,
            247089.0 / 357200.0,
            198249.0 / 2500400.0,
            0.0,
            32229.0 / 714400.0,
            5220557.0 / 5000800.0,
        ],
        r,
        g,
        b,
    )
}

fn xyz_to_lin_p3(x: F64, y: F64, z: F64) -> (F64, F64, F64) {
    multiply(
        [
            446124.0 / 178915.0,
            -333277.0 / 357830.0,
            -72051.0 / 178915.0,
            -14852.0 / 17905.0,
            63121.0 / 35810.0,
            423.0 / 17905.0,
            11844.0 / 330415.0,
            -50337.0 / 660830.0,
            316169.0 / 330415.0,
        ],
        x,
        y,
        z,
    )
}

fn lin_prophoto(r: F64, g: F64, b: F64) -> (F64, F64, F64) {
    map3(r, g, b, |value| {
        let abs = value.abs();
        if abs.value() <= 16.0 / 512.0 {
            value.div_const(16.0)
        } else {
            abs.pow_const(1.8).with_sign_from(value)
        }
    })
}

fn gam_prophoto(r: F64, g: F64, b: F64) -> (F64, F64, F64) {
    map3(r, g, b, |value| {
        let abs = value.abs();
        if abs.value() >= 1.0 / 512.0 {
            abs.pow_const(1.0 / 1.8).with_sign_from(value)
        } else {
            value.mul_const(16.0)
        }
    })
}

fn lin_prophoto_to_xyz(r: F64, g: F64, b: F64) -> (F64, F64, F64) {
    multiply(
        [
            0.7977604896723027,
            0.13518583717574031,
            0.0313493495815248,
            0.2880711282292934,
            0.7118432178101014,
            0.00008565396060525902,
            0.0,
            0.0,
            0.8251046025104601,
        ],
        r,
        g,
        b,
    )
}

fn xyz_to_lin_prophoto(x: F64, y: F64, z: F64) -> (F64, F64, F64) {
    multiply(
        [
            1.3457989731028281,
            -0.25558010007997534,
            -0.05110628506753401,
            -0.5446224939028347,
            1.5082327413132781,
            0.02053603239147973,
            0.0,
            0.0,
            1.2119675456389454,
        ],
        x,
        y,
        z,
    )
}

fn lin_a98(r: F64, g: F64, b: F64) -> (F64, F64, F64) {
    map3(r, g, b, |value| {
        value.abs().pow_const(563.0 / 256.0).with_sign_from(value)
    })
}

fn gam_a98(r: F64, g: F64, b: F64) -> (F64, F64, F64) {
    map3(r, g, b, |value| {
        value.abs().pow_const(256.0 / 563.0).with_sign_from(value)
    })
}

fn lin_a98_to_xyz(r: F64, g: F64, b: F64) -> (F64, F64, F64) {
    multiply(
        [
            573536.0 / 994567.0,
            263643.0 / 1420810.0,
            187206.0 / 994567.0,
            591459.0 / 1989134.0,
            6239551.0 / 9945670.0,
            374412.0 / 4972835.0,
            53769.0 / 1989134.0,
            351524.0 / 4972835.0,
            4929758.0 / 4972835.0,
        ],
        r,
        g,
        b,
    )
}

fn xyz_to_lin_a98(x: F64, y: F64, z: F64) -> (F64, F64, F64) {
    multiply(
        [
            1829569.0 / 896150.0,
            -506331.0 / 896150.0,
            -308931.0 / 896150.0,
            -851781.0 / 878810.0,
            1648619.0 / 878810.0,
            36519.0 / 878810.0,
            16779.0 / 1248040.0,
            -147721.0 / 1248040.0,
            1266979.0 / 1248040.0,
        ],
        x,
        y,
        z,
    )
}

fn lin_2020(r: F64, g: F64, b: F64) -> (F64, F64, F64) {
    const ALPHA: f64 = 1.09929682680944;
    const BETA: f64 = 0.018053968510807;
    map3(r, g, b, |value| {
        let abs = value.abs();
        if abs.value() < BETA * 4.5 {
            value.div_const(4.5)
        } else {
            abs.add_const(ALPHA - 1.0)
                .div_const(ALPHA)
                .pow_const(1.0 / 0.45)
                .with_sign_from(value)
        }
    })
}

fn gam_2020(r: F64, g: F64, b: F64) -> (F64, F64, F64) {
    const ALPHA: f64 = 1.09929682680944;
    const BETA: f64 = 0.018053968510807;
    map3(r, g, b, |value| {
        let abs = value.abs();
        if abs.value() > BETA {
            abs.pow_const(0.45)
                .mul_const(ALPHA)
                .sub_const(ALPHA - 1.0)
                .with_sign_from(value)
        } else {
            value.mul_const(4.5)
        }
    })
}

fn lin_2020_to_xyz(r: F64, g: F64, b: F64) -> (F64, F64, F64) {
    multiply(
        [
            63426534.0 / 99577255.0,
            20160776.0 / 139408157.0,
            47086771.0 / 278816314.0,
            26158966.0 / 99577255.0,
            472592308.0 / 697040785.0,
            8267143.0 / 139408157.0,
            0.0,
            19567812.0 / 697040785.0,
            295819943.0 / 278816314.0,
        ],
        r,
        g,
        b,
    )
}

fn xyz_to_lin_2020(x: F64, y: F64, z: F64) -> (F64, F64, F64) {
    multiply(
        [
            30757411.0 / 17917100.0,
            -6372589.0 / 17917100.0,
            -4539589.0 / 17917100.0,
            -19765991.0 / 29648200.0,
            47925759.0 / 29648200.0,
            467509.0 / 29648200.0,
            792561.0 / 44930125.0,
            -1921689.0 / 44930125.0,
            42328811.0 / 44930125.0,
        ],
        x,
        y,
        z,
    )
}

pub(super) fn d65_to_d50(x: F64, y: F64, z: F64) -> (F64, F64, F64) {
    multiply(
        [
            1.0479297925449969,
            0.022946870601609652,
            -0.05019226628920524,
            0.02962780877005599,
            0.9904344267538799,
            -0.017073799063418826,
            -0.009243040646204504,
            0.015055191490298152,
            0.7518742814281371,
        ],
        x,
        y,
        z,
    )
}

pub(super) fn d50_to_d65(x: F64, y: F64, z: F64) -> (F64, F64, F64) {
    multiply(
        [
            0.955473421488075,
            -0.02309845494876471,
            0.06325924320057072,
            -0.0283697093338637,
            1.0099953980813041,
            0.021041441191917323,
            0.012314014864481998,
            -0.020507649298898964,
            1.330365926242124,
        ],
        x,
        y,
        z,
    )
}

const D50_X: f64 = 0.3457 / 0.3585;
const D50_Z: f64 = (1.0 - 0.3457 - 0.3585) / 0.3585;

fn xyz_to_lab(mut x: F64, y: F64, mut z: F64) -> (F64, F64, F64) {
    const EPSILON: f64 = 216.0 / 24389.0;
    const KAPPA: f64 = 24389.0 / 27.0;
    x = x.div_const(D50_X);
    z = z.div_const(D50_Z);
    let f0 = if x.value() > EPSILON {
        x.cbrt()
    } else {
        x.mul_const(KAPPA).add_const(16.0).div_const(116.0)
    };
    let f1 = if y.value() > EPSILON {
        y.cbrt()
    } else {
        y.mul_const(KAPPA).add_const(16.0).div_const(116.0)
    };
    let f2 = if z.value() > EPSILON {
        z.cbrt()
    } else {
        z.mul_const(KAPPA).add_const(16.0).div_const(116.0)
    };
    (
        f1.mul_const(116.0).sub_const(16.0),
        f0.sub(f1).mul_const(500.0),
        f1.sub(f2).mul_const(200.0),
    )
}

pub(super) fn lab_to_xyz(l: F64, a: F64, b: F64) -> (F64, F64, F64) {
    const KAPPA: f64 = 24389.0 / 27.0;
    const EPSILON: f64 = 216.0 / 24389.0;
    let f1 = l.add_const(16.0).div_const(116.0);
    let f0 = a.div_const(500.0).add(f1);
    let f2 = f1.sub(b.div_const(200.0));
    let f0_3 = f0.cubed();
    let f2_3 = f2.cubed();
    let x = if f0_3.value() > EPSILON {
        f0_3
    } else {
        f0.mul_const(116.0).sub_const(16.0).div_const(KAPPA)
    };
    let y = if l.value() > KAPPA * EPSILON {
        l.add_const(16.0).div_const(116.0).cubed()
    } else {
        l.div_const(KAPPA)
    };
    let z = if f2_3.value() > EPSILON {
        f2_3
    } else {
        f2.mul_const(116.0).sub_const(16.0).div_const(KAPPA)
    };
    (x.mul_const(D50_X), y, z.mul_const(D50_Z))
}

fn lab_to_lch(l: F64, a: F64, b: F64) -> (F64, F64, F64) {
    let mut hue = b.atan2(a).mul_const(180.0 / std::f64::consts::PI);
    if hue.value() < 0.0 {
        hue = hue.add_const(360.0);
    }
    (l, a.squared().add(b.squared()).sqrt(), hue)
}

pub(super) fn lch_to_lab(l: F64, c: F64, h: F64) -> (F64, F64, F64) {
    let angle = h.mul_const(std::f64::consts::PI / 180.0);
    (l, angle.cos().mul(c), angle.sin().mul(c))
}

pub(super) fn xyz_to_oklab(x: F64, y: F64, z: F64) -> (F64, F64, F64) {
    let (l, m, s) = multiply(
        [
            0.8190224432164319,
            0.3619062562801221,
            -0.12887378261216414,
            0.0329836671980271,
            0.9292868468965546,
            0.03614466816999844,
            0.048177199566046255,
            0.26423952494422764,
            0.6335478258136937,
        ],
        x,
        y,
        z,
    );
    multiply(
        [
            0.2104542553,
            0.7936177850,
            -0.0040720468,
            1.9779984951,
            -2.4285922050,
            0.4505937099,
            0.0259040371,
            0.7827717662,
            -0.8086757660,
        ],
        l.cbrt(),
        m.cbrt(),
        s.cbrt(),
    )
}

pub(super) fn oklab_to_xyz(l: F64, a: F64, b: F64) -> (F64, F64, F64) {
    let (l, m, s) = multiply(
        [
            0.9999999984505198,
            0.39633779217376786,
            0.2158037580607588,
            1.0000000088817608,
            -0.10556134232365635,
            -0.0638541747717059,
            1.0000000546724109,
            -0.08948418209496576,
            -1.2914855378640917,
        ],
        l,
        a,
        b,
    );
    multiply(
        [
            1.2268798733741557,
            -0.5578149965554813,
            0.28139105017721583,
            -0.04057576262431372,
            1.1122868293970594,
            -0.07171106666151701,
            -0.07637294974672142,
            -0.4214933239627914,
            1.5869240244272418,
        ],
        l.cubed(),
        m.cubed(),
        s.cubed(),
    )
}

fn rgb_to_hsl(red: F64, green: F64, blue: F64) -> (F64, F64, F64) {
    let max = max3(red, green, blue);
    let min = min3(red, green, blue);
    let mut hue = F64::new(f64::NAN);
    let mut saturation = F64::new(0.0);
    let light = min.add(max).div_const(2.0);
    let delta = max.sub(min);
    if delta.value() != 0.0 {
        let divisor = min2(light, light.negated().add_const(1.0));
        if divisor.value() != 0.0 {
            saturation = max.sub(light).div(divisor);
        }
        if max == red {
            hue = green.sub(blue).div(delta);
            if green.value() < blue.value() {
                hue = hue.add_const(6.0);
            }
        } else if max == green {
            hue = blue.sub(red).div(delta).add_const(2.0);
        } else {
            hue = red.sub(green).div(delta).add_const(4.0);
        }
        hue = hue.mul_const(60.0);
    }
    (hue, saturation.mul_const(100.0), light.mul_const(100.0))
}

pub(super) fn hsl_to_rgb(mut hue: F64, mut saturation: F64, mut light: F64) -> (F64, F64, F64) {
    hue = hue.div_const(360.0);
    hue = hue.sub(hue.floor()).mul_const(360.0);
    saturation = saturation.div_const(100.0);
    light = light.div_const(100.0);
    let f = |n: f64| {
        let mut k = hue.div_const(30.0).add_const(n);
        k = k.div_const(12.0);
        k = k.sub(k.floor()).mul_const(12.0);
        let a = min2(light, light.negated().add_const(1.0)).mul(saturation);
        light.sub(
            max2(
                F64::new(-1.0),
                min3(k.sub_const(3.0), k.negated().add_const(9.0), F64::new(1.0)),
            )
            .mul(a),
        )
    };
    (f(0.0), f(8.0), f(4.0))
}

fn rgb_to_hwb(red: F64, green: F64, blue: F64) -> (F64, F64, F64) {
    let (hue, _, _) = rgb_to_hsl(red, green, blue);
    (
        hue,
        min3(red, green, blue).mul_const(100.0),
        max3(red, green, blue)
            .negated()
            .add_const(1.0)
            .mul_const(100.0),
    )
}

pub(super) fn hwb_to_rgb(hue: F64, mut white: F64, mut black: F64) -> (F64, F64, F64) {
    white = white.div_const(100.0);
    black = black.div_const(100.0);
    if white.add(black).value() >= 1.0 {
        let gray = white.div(white.add(black));
        return (gray, gray, gray);
    }
    let delta = white.add(black).negated().add_const(1.0);
    let (r, g, b) = hsl_to_rgb(hue, F64::new(100.0), F64::new(50.0));
    (
        delta.mul(r).add(white),
        delta.mul(g).add(white),
        delta.mul(b).add(white),
    )
}

pub(super) fn xyz_to_color_space(x: F64, y: F64, z: F64, space: ColorSpace) -> (F64, F64, F64) {
    match space {
        ColorSpace::A98Rgb => apply3(xyz_to_lin_a98(x, y, z), gam_a98),
        ColorSpace::DisplayP3 => apply3(xyz_to_lin_p3(x, y, z), gam_srgb),
        ColorSpace::Hsl => apply3(apply3(xyz_to_lin_srgb(x, y, z), gam_srgb), rgb_to_hsl),
        ColorSpace::Hwb => apply3(apply3(xyz_to_lin_srgb(x, y, z), gam_srgb), rgb_to_hwb),
        ColorSpace::Lab => apply3(d65_to_d50(x, y, z), xyz_to_lab),
        ColorSpace::Lch => apply3(apply3(d65_to_d50(x, y, z), xyz_to_lab), lab_to_lch),
        ColorSpace::Oklab => xyz_to_oklab(x, y, z),
        ColorSpace::Oklch => apply3(xyz_to_oklab(x, y, z), lab_to_lch),
        ColorSpace::ProphotoRgb => apply3(
            apply3(d65_to_d50(x, y, z), xyz_to_lin_prophoto),
            gam_prophoto,
        ),
        ColorSpace::Rec2020 => apply3(xyz_to_lin_2020(x, y, z), gam_2020),
        ColorSpace::Srgb => apply3(xyz_to_lin_srgb(x, y, z), gam_srgb),
        ColorSpace::SrgbLinear => xyz_to_lin_srgb(x, y, z),
        ColorSpace::Xyz | ColorSpace::XyzD65 => (x, y, z),
        ColorSpace::XyzD50 => d65_to_d50(x, y, z),
    }
}

pub(super) fn color_space_to_xyz(a: F64, b: F64, c: F64, space: ColorSpace) -> (F64, F64, F64) {
    match space {
        ColorSpace::A98Rgb => apply3(lin_a98(a, b, c), lin_a98_to_xyz),
        ColorSpace::DisplayP3 => apply3(lin_srgb(a, b, c), lin_p3_to_xyz),
        ColorSpace::Hsl => apply3(apply3(hsl_to_rgb(a, b, c), lin_srgb), lin_srgb_to_xyz),
        ColorSpace::Hwb => apply3(apply3(hwb_to_rgb(a, b, c), lin_srgb), lin_srgb_to_xyz),
        ColorSpace::Lab => apply3(lab_to_xyz(a, b, c), d50_to_d65),
        ColorSpace::Lch => apply3(apply3(lch_to_lab(a, b, c), lab_to_xyz), d50_to_d65),
        ColorSpace::Oklab => oklab_to_xyz(a, b, c),
        ColorSpace::Oklch => apply3(lch_to_lab(a, b, c), oklab_to_xyz),
        ColorSpace::ProphotoRgb => apply3(
            apply3(lin_prophoto(a, b, c), lin_prophoto_to_xyz),
            d50_to_d65,
        ),
        ColorSpace::Rec2020 => apply3(lin_2020(a, b, c), lin_2020_to_xyz),
        ColorSpace::Srgb => apply3(lin_srgb(a, b, c), lin_srgb_to_xyz),
        ColorSpace::SrgbLinear => lin_srgb_to_xyz(a, b, c),
        ColorSpace::Xyz | ColorSpace::XyzD65 => (a, b, c),
        ColorSpace::XyzD50 => d50_to_d65(a, b, c),
    }
}

fn delta_e_ok(l1: F64, a1: F64, b1: F64, l2: F64, a2: F64, b2: F64) -> F64 {
    l1.sub(l2)
        .squared()
        .add(a1.sub(a2).squared())
        .add(b1.sub(b2).squared())
        .sqrt()
}

pub(super) fn gamut_mapping_xyz_to_srgb(x: F64, y: F64, z: F64) -> (F64, F64, F64) {
    let (origin_l, mut origin_c, origin_h) = apply3(xyz_to_oklab(x, y, z), lab_to_lch);
    if origin_l.value() >= 1.0 || origin_l.value() <= 0.0 {
        return (origin_l, origin_l, origin_l);
    }
    let to_srgb = |l, c, h| {
        let (l, a, b) = lch_to_lab(l, c, h);
        apply3(apply3(oklab_to_xyz(l, a, b), xyz_to_lin_srgb), gam_srgb)
    };
    let to_oklab = |r, g, b| apply3(apply3(lin_srgb(r, g, b), lin_srgb_to_xyz), xyz_to_oklab);
    let in_gamut = |r: F64, g: F64, b: F64| {
        [r, g, b]
            .into_iter()
            .all(|v| (0.0..=1.0).contains(&v.value()))
    };
    let (mut r, mut g, mut b) = to_srgb(origin_l, origin_c, origin_h);
    if in_gamut(r, g, b) {
        return (r, g, b);
    }
    let mut min = F64::new(0.0);
    let mut max = origin_c;
    let clip = |v: F64| F64::new(v.value().clamp(0.0, 1.0));
    while max.sub(min).value() > 0.0001 {
        let chroma = min.add(max).div_const(2.0);
        origin_c = chroma;
        (r, g, b) = to_srgb(origin_l, origin_c, origin_h);
        if in_gamut(r, g, b) {
            min = chroma;
            continue;
        }
        let (cr, cg, cb) = (clip(r), clip(g), clip(b));
        // Keep this upstream channel order, including its historical swap.
        let (l1, a1, b1) = to_oklab(cr, cb, cg);
        let (l2, a2, b2) = to_oklab(r, g, b);
        if delta_e_ok(l1, a1, b1, l2, a2, b2).value() < 0.02 {
            return (cr, cg, cb);
        }
        max = chroma;
    }
    (r, g, b)
}

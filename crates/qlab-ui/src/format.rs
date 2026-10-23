//! Formato y análisis de tiempos y niveles, como los muestra QLab.

/// `mm:ss.cc` (centésimas); pasa a `h:mm:ss` desde la hora.
pub fn fmt_time(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return "--:--.--".into();
    }
    let total_cs = (secs * 100.0).round() as u64;
    let (cs, s_total) = (total_cs % 100, total_cs / 100);
    let (s, m_total) = (s_total % 60, s_total / 60);
    if m_total >= 60 {
        format!("{}:{:02}:{:02}", m_total / 60, m_total % 60, s)
    } else {
        format!("{:02}:{:02}.{:02}", m_total, s, cs)
    }
}

/// Acepta `1.5`, `0:01.50`, `1:02:03.5` y `90s`.
pub fn parse_time(text: &str) -> Option<f64> {
    let t = text.trim().trim_end_matches(['s', 'S']).trim();
    if t.is_empty() {
        return None;
    }
    let mut total = 0.0;
    let parts: Vec<&str> = t.split(':').collect();
    if parts.len() > 3 {
        return None;
    }
    for p in &parts {
        let v: f64 = p.trim().replace(',', ".").parse().ok()?;
        if !v.is_finite() || v < 0.0 {
            return None;
        }
        total = total * 60.0 + v;
    }
    Some(total)
}

pub const MIN_DB: f32 = -120.0;
pub const MAX_DB: f32 = 12.0;

/// `-inf` en el suelo del rango; un decimal en el resto.
pub fn fmt_db(db: f32) -> String {
    if db.is_nan() || db <= MIN_DB {
        "-inf".into()
    } else if db > 0.0 {
        format!("+{db:.1}")
    } else {
        format!("{db:.1}")
    }
}

/// Acepta `-6`, `-6.5 dB`, `+3`, `-inf` y `inf`/`-∞`; recorta a `[-120, +12]`.
pub fn parse_db(text: &str) -> Option<f32> {
    let t = text.trim().to_lowercase();
    let t = t.trim_end_matches("db").trim();
    if matches!(t, "-inf" | "-∞" | "inf" | "∞" | "-infinity" | "silence" | "off") {
        return Some(MIN_DB);
    }
    let v: f32 = t.replace(',', ".").parse().ok()?;
    if v.is_nan() {
        return None;
    }
    Some(v.clamp(MIN_DB, MAX_DB))
}

/// Posición 0..=1 de un nivel en un fader con escala de audio: `-inf` abajo, 0 dB a ~83 %, +12 arriba.
pub fn db_to_fader(db: f32) -> f32 {
    if db <= MIN_DB {
        return 0.0;
    }
    if db >= MAX_DB {
        return 1.0;
    }
    // Tramo -60..+12 dB lineal; por debajo de -60 cae hasta -inf en el último 10 %.
    if db >= -60.0 {
        0.10 + 0.90 * (db.min(MAX_DB) + 60.0) / 72.0
    } else {
        0.10 * ((db - MIN_DB) / (-60.0 - MIN_DB)).clamp(0.0, 1.0)
    }
}

pub fn fader_to_db(pos: f32) -> f32 {
    let p = pos.clamp(0.0, 1.0);
    if p <= 0.0 {
        MIN_DB
    } else if p >= 0.10 {
        -60.0 + (p - 0.10) / 0.90 * 72.0
    } else {
        MIN_DB + p / 0.10 * (-60.0 - MIN_DB)
    }
}

/// Recorta un texto largo con puntos suspensivos (por caracteres, no por bytes).
pub fn ellipsize(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let keep: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{keep}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_formatting() {
        assert_eq!(fmt_time(0.0), "00:00.00");
        assert_eq!(fmt_time(1.5), "00:01.50");
        assert_eq!(fmt_time(61.234), "01:01.23");
        assert_eq!(fmt_time(3_725.0), "1:02:05");
        assert_eq!(fmt_time(-1.0), "--:--.--");
        assert_eq!(fmt_time(f64::NAN), "--:--.--");
        assert_eq!(fmt_time(59.999), "01:00.00");
    }

    #[test]
    fn time_parsing() {
        assert_eq!(parse_time("1.5"), Some(1.5));
        assert_eq!(parse_time(" 0:01.50 "), Some(1.5));
        assert_eq!(parse_time("1:02:03"), Some(3_723.0));
        assert_eq!(parse_time("90s"), Some(90.0));
        assert_eq!(parse_time("2,5"), Some(2.5));
        for bad in ["", "abc", "-1", "1:2:3:4", "1::2", "nan"] {
            assert_eq!(parse_time(bad), None, "{bad:?}");
        }
        for t in [0.0, 1.5, 61.25, 3_723.0] {
            assert!((parse_time(&fmt_time(t)).unwrap() - t).abs() < 0.006);
        }
    }

    #[test]
    fn db_formatting_and_parsing() {
        assert_eq!(fmt_db(-120.0), "-inf");
        assert_eq!(fmt_db(f32::NEG_INFINITY), "-inf");
        assert_eq!(fmt_db(0.0), "0.0");
        assert_eq!(fmt_db(-6.04), "-6.0");
        assert_eq!(fmt_db(3.0), "+3.0");
        assert_eq!(parse_db("-6"), Some(-6.0));
        assert_eq!(parse_db(" +3.5 dB "), Some(3.5));
        assert_eq!(parse_db("-inf"), Some(-120.0));
        assert_eq!(parse_db("-∞"), Some(-120.0));
        assert_eq!(parse_db("99"), Some(12.0));
        assert_eq!(parse_db("-500"), Some(-120.0));
        assert_eq!(parse_db("2,5"), Some(2.5));
        assert_eq!(parse_db("hola"), None);
        assert_eq!(parse_db("nan"), None);
    }

    #[test]
    fn fader_taper_is_monotonic_and_invertible() {
        assert_eq!(db_to_fader(-120.0), 0.0);
        assert_eq!(db_to_fader(12.0), 1.0);
        assert!((db_to_fader(0.0) - (0.10 + 0.90 * 60.0 / 72.0)).abs() < 1e-6);
        let mut prev = -1.0;
        for i in 0..=1_320 {
            let db = -120.0 + i as f32 * 0.1;
            let p = db_to_fader(db);
            assert!(p >= prev, "no monótono en {db}");
            prev = p;
            if db > -119.9 {
                assert!((fader_to_db(p) - db).abs() < 0.05, "{db} → {p} → {}", fader_to_db(p));
            }
        }
        assert_eq!(fader_to_db(0.0), -120.0);
        assert_eq!(fader_to_db(7.0), 12.0);
    }

    #[test]
    fn ellipsis_respects_characters() {
        assert_eq!(ellipsize("hola", 10), "hola");
        assert_eq!(ellipsize("canción de cuna", 8), "canción…");
        assert_eq!(ellipsize("ñññññ", 3).chars().count(), 3);
    }
}

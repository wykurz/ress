//! Deterministic log-like fixture generation: same `Spec` (including seed) —
//! same bytes, on every platform, with no external PRNG dependency.
/// What to generate. `target_bytes` is a ceiling: generation stops at the
/// last whole line that fits (a single truncated line is emitted when even
/// the first line would not fit, so output is never empty).
#[derive(Clone, Copy)]
pub struct Spec {
    pub seed: u64,
    pub target_bytes: u64,
    pub line_len: LineLen,
    pub trailing_newline: bool,
    pub mega: Option<Mega>,
    pub utf8_fraction: f64,
}
/// Per-line payload length in BYTES (the newline is not counted).
#[derive(Clone, Copy)]
pub enum LineLen {
    Fixed(usize),
    /// Drawn uniformly from `[min, max)` — `max` is exclusive, so a line's
    /// length never equals `max`. `min == max` (not just `min > max`) is
    /// rejected by [`validate`], since a half-open range with `min == max`
    /// is empty rather than a degenerate fixed-length case.
    Uniform {
        min: usize,
        max: usize,
    },
}
/// Every `every`-th line (1-based) carries a `len`-byte payload instead of
/// its drawn length — the embedded mega-line case from the design spec.
#[derive(Clone, Copy)]
pub struct Mega {
    pub every: u64,
    pub len: usize,
}
#[derive(Debug)]
pub struct Stats {
    pub bytes: u64,
    pub lines: u64,
}
// splitmix64: tiny, seedable, platform-stable; deliberately no `rand` dep.
struct SplitMix64(u64);
impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        // modulo bias is irrelevant for fixture variety
        self.next() % n.max(1)
    }
}
const ASCII_PALETTE: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789 .:-_/";
const UTF8_PALETTE: &[&str] = &["\u{e9}", "\u{fc}", "\u{4e16}", "\u{754c}", "\u{2192}"];
// u64 -> usize, saturating rather than truncating: on a 32-bit target,
// target_bytes above usize::MAX must still act as an effective ceiling
// (usize::MAX itself), never wrap down to some small, wrong value. this
// crate's whole promise is "same Spec, same bytes, on every platform" (see
// the module doc comment); a plain `as usize` here would make a
// target_bytes-dependent output silently diverge specifically on 32-bit
// platforms, which nothing downstream would catch (the real per-write
// budget check stays in u64 throughout generate(), so this clamp only
// bounds how large a single line's buffer gets built, never the actual
// byte count written).
fn usize_ceiling(target_bytes: u64) -> usize {
    usize::try_from(target_bytes).unwrap_or(usize::MAX)
}
/// Rejects a `Spec` that would misbehave once generation starts, before any
/// byte is written (and, for the CLI, before the destination is opened or
/// truncated). `generate()` calls this first; the CLI also calls it itself,
/// between assembling the final `Spec` and creating the output file, so a
/// bad spec is rejected before the destination is touched at all.
pub fn validate(spec: &Spec) -> std::io::Result<()> {
    // target_bytes == 0 is not "generate an empty file": the loop below
    // always attempts one line before checking whether anything fit, so a
    // zero-byte ceiling leaves it counting a line that was never written
    // (trailing_newline: false) or writing a trailing newline the ceiling
    // had no room for (trailing_newline: true). rejecting it here makes
    // both inconsistent outcomes unrepresentable rather than patching the
    // bookkeeping to special-case a target with no capacity for anything.
    if spec.target_bytes == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "target_bytes must be nonzero",
        ));
    }
    if let LineLen::Uniform { min, max } = spec.line_len
        && min >= max
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("line_len::uniform requires min < max (min={min}, max={max})"),
        ));
    }
    // every == 0 does not panic (u64::is_multiple_of special-cases rhs == 0
    // as `self == 0`, never dividing), but it is not a meaningful "disable
    // mega" spelling either: "every 0th line" selects no line, ever, so the
    // predicate silently never fires and mega is dropped without a trace.
    // rejecting it is the same unrepresentable-by-construction move as the
    // two checks above, not a new kind of check.
    if let Some(m) = &spec.mega
        && m.every == 0
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "mega.every must be nonzero",
        ));
    }
    // a fraction is only meaningful in [0.0, 1.0]; RangeInclusive::contains
    // rejects NaN on its own (every comparison against NaN is false, so
    // `contains` returns false without special-casing it), and rejects
    // +-infinity the same way, so one check covers every non-finite and
    // out-of-range value without a separate is_finite() call.
    if !(0.0..=1.0).contains(&spec.utf8_fraction) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "utf8_fraction must be within 0.0..=1.0 (got {})",
                spec.utf8_fraction
            ),
        ));
    }
    Ok(())
}
pub fn generate(spec: &Spec, out: &mut dyn std::io::Write) -> std::io::Result<Stats> {
    validate(spec)?;
    let mut rng = SplitMix64(spec.seed);
    let mut bytes: u64 = 0;
    let mut lines: u64 = 0;
    let mut buf: Vec<u8> = Vec::new();
    // newlines are separators between lines, not per-line terminators: the
    // final trailing newline (if any) is reserved up front and appended once
    // after the loop, so no iteration has to guess whether its line is last.
    let content_budget = spec
        .target_bytes
        .saturating_sub(u64::from(spec.trailing_newline));
    loop {
        let drawn = match spec.line_len {
            LineLen::Fixed(n) => n,
            LineLen::Uniform { min, max } => min + rng.below((max - min) as u64) as usize,
        };
        let payload_len = match &spec.mega {
            Some(m) if (lines + 1).is_multiple_of(m.every) => m.len,
            _ => drawn,
        };
        // never build more than the target can hold: an absurdly long drawn
        // length (e.g. single-line's `usize::MAX`) must not blow up memory —
        // anything beyond target_bytes gets truncated away anyway, so cap
        // the fill at that ceiling to keep this call O(target_bytes).
        let payload_len = payload_len.min(usize_ceiling(spec.target_bytes));
        let is_first = lines == 0;
        let separator = u64::from(!is_first);
        // for a non-first line, fill_line's own output length is always
        // exactly payload_len (its trailing truncate(len) guarantees that),
        // so whether this line fits is already knowable from payload_len
        // alone — checking it here, before fill_line runs, means a huge
        // mega-line drawn near the end of a much smaller remaining budget
        // (payload_len is capped by the WHOLE target above, not by what is
        // actually left) never gets materialized just to be thrown away.
        // the first line is exempt: its own too-big-to-fit case still needs
        // a real buffer to cut from (the truncation fallback just below), so
        // it always builds one; that path's payload_len is already bounded
        // by target_bytes via the clamp above, so it was never the source of
        // unbounded waste. generation ends at this break either way — no
        // later line is ever drawn from whatever rng state a skipped
        // fill_line call would have left, so skipping it cannot change one
        // byte of what was already written, or what any earlier line was.
        if !is_first && bytes + separator + payload_len as u64 > content_budget {
            break;
        }
        let multibyte =
            spec.utf8_fraction > 0.0 && (rng.next() as f64 / u64::MAX as f64) < spec.utf8_fraction;
        fill_line(&mut buf, lines + 1, payload_len, multibyte, &mut rng);
        if is_first && bytes + separator + buf.len() as u64 > content_budget {
            // not even one line fits: emit a truncated first line filling
            // the target exactly, newline last iff the policy asks for one.
            // buf may hold multi-byte utf-8 (fill_line only ever appends
            // whole chars), so the cut is floored to the nearest char
            // boundary at or before `body`, then padded with single-byte
            // ascii to still land exactly on the byte budget — cutting at
            // an arbitrary byte offset would risk splitting a character.
            let take = usize_ceiling(spec.target_bytes);
            let body = take.saturating_sub(usize::from(spec.trailing_newline));
            let mut cut = body.min(buf.len());
            while cut > 0 && buf[cut] & 0xc0 == 0x80 {
                cut -= 1;
            }
            out.write_all(&buf[..cut])?;
            for _ in cut..body {
                out.write_all(b" ")?;
            }
            if spec.trailing_newline {
                out.write_all(b"\n")?;
            }
            return Ok(Stats {
                bytes: take as u64,
                lines: 1,
            });
        }
        if !is_first {
            out.write_all(b"\n")?;
            bytes += 1;
        }
        out.write_all(&buf)?;
        bytes += buf.len() as u64;
        lines += 1;
    }
    if spec.trailing_newline {
        out.write_all(b"\n")?;
        bytes += 1;
    }
    Ok(Stats { bytes, lines })
}
fn fill_line(buf: &mut Vec<u8>, line_no: u64, len: usize, multibyte: bool, rng: &mut SplitMix64) {
    buf.clear();
    let prefix = format!("{line_no:010} ");
    buf.extend_from_slice(prefix.as_bytes());
    while buf.len() < len {
        let remaining = len - buf.len();
        if multibyte && remaining >= 3 && rng.below(4) == 0 {
            let ch = UTF8_PALETTE[rng.below(UTF8_PALETTE.len() as u64) as usize];
            if ch.len() <= remaining {
                buf.extend_from_slice(ch.as_bytes());
                continue;
            }
        }
        buf.push(ASCII_PALETTE[rng.below(ASCII_PALETTE.len() as u64) as usize]);
    }
    buf.truncate(len);
}
pub const PRESETS: &[&str] = &[
    "uniform-log",
    "varied-log",
    "megalines",
    "utf8-heavy",
    "no-trailing-newline",
    "single-line",
];
pub fn preset(name: &str) -> Option<Spec> {
    let base = Spec {
        seed: 0xC0FFEE,
        target_bytes: 1 << 30,
        line_len: LineLen::Uniform { min: 40, max: 120 },
        trailing_newline: true,
        mega: None,
        utf8_fraction: 0.0,
    };
    match name {
        "uniform-log" => Some(base),
        "varied-log" => Some(Spec {
            line_len: LineLen::Uniform { min: 10, max: 200 },
            utf8_fraction: 0.1,
            ..base
        }),
        "megalines" => Some(Spec {
            mega: Some(Mega {
                every: 1000,
                len: 1 << 20,
            }),
            ..base
        }),
        "utf8-heavy" => Some(Spec {
            line_len: LineLen::Uniform { min: 20, max: 150 },
            utf8_fraction: 0.8,
            ..base
        }),
        "no-trailing-newline" => Some(Spec {
            trailing_newline: false,
            ..base
        }),
        "single-line" => Some(Spec {
            line_len: LineLen::Fixed(usize::MAX),
            trailing_newline: false,
            ..base
        }),
        _ => None,
    }
}
#[cfg(test)]
mod tests {
    #[test]
    fn same_spec_same_bytes() {
        let spec = crate::Spec {
            seed: 42,
            target_bytes: 1 << 20,
            line_len: crate::LineLen::Uniform { min: 10, max: 200 },
            trailing_newline: true,
            mega: None,
            utf8_fraction: 0.1,
        };
        let mut a = Vec::new();
        let mut b = Vec::new();
        let sa = crate::generate(&spec, &mut a).unwrap();
        let sb = crate::generate(&spec, &mut b).unwrap();
        assert_eq!(a, b);
        assert_eq!(sa.bytes, sb.bytes);
        assert_eq!(sa.lines, sb.lines);
        assert_eq!(sa.bytes, a.len() as u64);
    }
    #[test]
    fn different_seed_different_bytes() {
        let base = crate::Spec {
            seed: 1,
            target_bytes: 1 << 16,
            line_len: crate::LineLen::Uniform { min: 10, max: 80 },
            trailing_newline: true,
            mega: None,
            utf8_fraction: 0.0,
        };
        let other = crate::Spec { seed: 2, ..base };
        let mut a = Vec::new();
        let mut b = Vec::new();
        crate::generate(&base, &mut a).unwrap();
        crate::generate(&other, &mut b).unwrap();
        assert_ne!(a, b);
    }
    #[test]
    fn size_never_exceeds_target_and_stats_count_lines() {
        let spec = crate::Spec {
            seed: 7,
            target_bytes: 100_000,
            line_len: crate::LineLen::Uniform { min: 10, max: 200 },
            trailing_newline: true,
            mega: None,
            utf8_fraction: 0.2,
        };
        let mut out = Vec::new();
        let stats = crate::generate(&spec, &mut out).unwrap();
        assert!(out.len() as u64 <= spec.target_bytes);
        let newlines = out.iter().filter(|b| **b == b'\n').count() as u64;
        assert_eq!(stats.lines, newlines);
        assert_eq!(*out.last().unwrap(), b'\n');
    }
    #[test]
    fn no_trailing_newline_ends_mid_line() {
        let spec = crate::Spec {
            seed: 7,
            target_bytes: 10_000,
            line_len: crate::LineLen::Fixed(50),
            trailing_newline: false,
            mega: None,
            utf8_fraction: 0.0,
        };
        let mut out = Vec::new();
        let stats = crate::generate(&spec, &mut out).unwrap();
        assert_ne!(*out.last().unwrap(), b'\n');
        // the unterminated final line still counts as a line
        let newlines = out.iter().filter(|b| **b == b'\n').count() as u64;
        assert_eq!(stats.lines, newlines + 1);
    }
    #[test]
    fn tiny_target_still_emits_one_line() {
        let spec = crate::Spec {
            seed: 3,
            target_bytes: 5,
            line_len: crate::LineLen::Fixed(1000),
            trailing_newline: true,
            mega: None,
            utf8_fraction: 0.0,
        };
        let mut out = Vec::new();
        let stats = crate::generate(&spec, &mut out).unwrap();
        assert_eq!(out.len(), 5);
        assert_eq!(stats.lines, 1);
        assert_eq!(*out.last().unwrap(), b'\n');
    }
    #[test]
    fn uniform_bounds_hold_for_every_line() {
        let spec = crate::Spec {
            seed: 11,
            target_bytes: 200_000,
            line_len: crate::LineLen::Uniform { min: 30, max: 60 },
            trailing_newline: true,
            mega: None,
            utf8_fraction: 0.0,
        };
        let mut out = Vec::new();
        crate::generate(&spec, &mut out).unwrap();
        for line in out.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
            assert!(
                line.len() >= 30 && line.len() < 60,
                "line len {}",
                line.len()
            );
        }
    }
    #[test]
    fn mega_lines_land_on_schedule_with_requested_length() {
        let spec = crate::Spec {
            seed: 5,
            target_bytes: 2_000_000,
            line_len: crate::LineLen::Fixed(50),
            trailing_newline: true,
            mega: Some(crate::Mega {
                every: 100,
                len: 100_000,
            }),
            utf8_fraction: 0.0,
        };
        let mut out = Vec::new();
        crate::generate(&spec, &mut out).unwrap();
        let lines: Vec<&[u8]> = out
            .split(|b| *b == b'\n')
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(
            lines[99].len(),
            100_000,
            "line 100 (1-based) is the mega line"
        );
        assert_eq!(lines[0].len(), 50);
    }
    #[test]
    fn utf8_fraction_one_yields_valid_multibyte_lines() {
        let spec = crate::Spec {
            seed: 9,
            target_bytes: 50_000,
            line_len: crate::LineLen::Uniform { min: 20, max: 120 },
            trailing_newline: true,
            mega: None,
            utf8_fraction: 1.0,
        };
        let mut out = Vec::new();
        crate::generate(&spec, &mut out).unwrap();
        let text = std::str::from_utf8(&out).expect("output must be valid utf-8");
        assert!(!text.is_ascii(), "multibyte content expected");
    }
    #[test]
    fn every_preset_generates() {
        for name in crate::PRESETS {
            let mut spec = crate::preset(name).expect("named preset exists");
            spec.target_bytes = 1 << 16;
            let mut out = Vec::new();
            let stats = crate::generate(&spec, &mut out).unwrap();
            assert!(stats.bytes > 0, "preset {name} produced bytes");
        }
        assert!(crate::preset("no-such").is_none());
    }
    #[test]
    fn single_line_preset_stays_bounded_and_has_no_interior_newlines() {
        // LineLen::Fixed(usize::MAX) must not attempt a usize::MAX-byte
        // allocation; the payload_len clamp in generate() keeps this O(target_bytes)
        // regardless of which of its two write paths ends up handling the line.
        let mut spec = crate::preset("single-line").expect("single-line preset exists");
        spec.target_bytes = 1 << 20;
        let mut out = Vec::new();
        let stats = crate::generate(&spec, &mut out).unwrap();
        assert_eq!(out.len(), 1 << 20);
        assert_eq!(stats.bytes, 1 << 20);
        assert_eq!(stats.lines, 1);
        assert!(
            !out.contains(&b'\n'),
            "single-line output must contain no newlines"
        );
    }
    #[test]
    fn uniform_min_ge_max_is_rejected_before_any_write() {
        // max is exclusive: min == max is also invalid, not just min > max.
        let make = |min, max| crate::Spec {
            seed: 1,
            target_bytes: 1024,
            line_len: crate::LineLen::Uniform { min, max },
            trailing_newline: true,
            mega: None,
            utf8_fraction: 0.0,
        };
        let mut out = Vec::new();
        let err = crate::generate(&make(20, 10), &mut out).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        let err = crate::generate(&make(10, 10), &mut out).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(
            out.is_empty(),
            "no byte should be written before validation"
        );
    }
    #[test]
    fn zero_target_bytes_is_rejected_before_any_write() {
        let make = |trailing_newline| crate::Spec {
            seed: 1,
            target_bytes: 0,
            line_len: crate::LineLen::Fixed(10),
            trailing_newline,
            mega: None,
            utf8_fraction: 0.0,
        };
        let mut out = Vec::new();
        let err = crate::generate(&make(true), &mut out).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        let err = crate::generate(&make(false), &mut out).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(
            out.is_empty(),
            "no byte should be written before validation"
        );
    }
    #[test]
    fn mega_every_zero_is_rejected_before_any_write() {
        // Mega{every: 0} does not panic (u64::is_multiple_of special-cases
        // rhs == 0 as `self == 0`, never dividing): pre-fix, this silently
        // disabled mega entirely (every drawn line, including line 1, takes
        // the ordinary path) instead of rejecting a config that can never
        // mean anything, since "every 0th line" has no line to select.
        let spec = crate::Spec {
            seed: 1,
            target_bytes: 1024,
            line_len: crate::LineLen::Fixed(10),
            trailing_newline: true,
            mega: Some(crate::Mega { every: 0, len: 5 }),
            utf8_fraction: 0.0,
        };
        let mut out = Vec::new();
        let err = crate::generate(&spec, &mut out).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(
            out.is_empty(),
            "no byte should be written before validation"
        );
    }
    #[test]
    fn utf8_fraction_out_of_range_is_rejected_before_any_write() {
        // a fraction is only meaningful in [0.0, 1.0]; NaN is the classic
        // footgun for a hand-rolled range check (every comparison with NaN
        // is false), so it is exercised explicitly here rather than trusted
        // to fall out of the other cases.
        let make = |utf8_fraction| crate::Spec {
            seed: 1,
            target_bytes: 1024,
            line_len: crate::LineLen::Fixed(10),
            trailing_newline: true,
            mega: None,
            utf8_fraction,
        };
        for bad in [-0.1, 1.1, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut out = Vec::new();
            let err = crate::generate(&make(bad), &mut out).unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "value: {bad}");
            assert!(
                out.is_empty(),
                "no byte should be written before validation (value: {bad})"
            );
        }
        // the boundary values themselves must still be accepted.
        for ok in [0.0, 1.0] {
            let mut out = Vec::new();
            crate::generate(&make(ok), &mut out).unwrap();
        }
    }
    #[test]
    fn truncated_first_line_is_valid_utf8_and_exact_size() {
        // the reported repro: `--fixed-len 2000 --utf8-fraction 1 --kib 1
        // --seed 2` used to cut mid-character at byte 1021, an invalid-utf8
        // output — this pins the exact Spec that produced it.
        let spec = crate::Spec {
            seed: 2,
            target_bytes: 1024,
            line_len: crate::LineLen::Fixed(2000),
            trailing_newline: true,
            mega: None,
            utf8_fraction: 1.0,
        };
        let mut out = Vec::new();
        let stats = crate::generate(&spec, &mut out).unwrap();
        assert_eq!(out.len(), 1024);
        assert_eq!(stats.bytes, 1024);
        std::str::from_utf8(&out).expect("output must be valid utf-8");
    }
    #[test]
    fn tiny_targets_with_multibyte_content_stay_valid_utf8_and_exact_size() {
        // line_len is always far bigger than target_bytes here, so every
        // combination forces the truncated-first-line fallback path,
        // sweeping the char-boundary floor across many cut points.
        for target_bytes in 1u64..=80 {
            for trailing_newline in [true, false] {
                let spec = crate::Spec {
                    seed: 2,
                    target_bytes,
                    line_len: crate::LineLen::Fixed(2000),
                    trailing_newline,
                    mega: None,
                    utf8_fraction: 1.0,
                };
                let mut out = Vec::new();
                let stats = crate::generate(&spec, &mut out).unwrap();
                assert_eq!(out.len() as u64, target_bytes);
                assert_eq!(stats.bytes, target_bytes);
                std::str::from_utf8(&out).unwrap_or_else(|e| {
                    panic!("target_bytes={target_bytes} trailing_newline={trailing_newline}: {e}")
                });
            }
        }
    }
    #[test]
    fn oversized_mega_line_stops_generation_before_it_no_longer_fits() {
        // a mega line's payload_len is clamped only by target_bytes overall,
        // not by whatever budget actually remains at that point in the loop
        // — so a mega line due late, when little budget is left, must be
        // rejected on length alone, before fill_line ever builds it (the
        // pre-fix bug built it anyway, then discarded the result). line 1 and
        // 3 are ordinary 50-byte lines; line 2's 1800-byte mega fits inside
        // the 2000-byte target; line 4's identical 1800-byte mega does not
        // (only ~98 bytes remain), so generation stops there — three lines
        // total, not four, and the byte budget is never exceeded.
        let spec = crate::Spec {
            seed: 1,
            target_bytes: 2000,
            line_len: crate::LineLen::Fixed(50),
            trailing_newline: true,
            mega: Some(crate::Mega {
                every: 2,
                len: 1800,
            }),
            utf8_fraction: 0.0,
        };
        let mut out = Vec::new();
        let stats = crate::generate(&spec, &mut out).unwrap();
        assert!(out.len() as u64 <= spec.target_bytes);
        assert_eq!(stats.bytes, out.len() as u64);
        assert_eq!(stats.lines, 3);
        let lines: Vec<&[u8]> = out
            .split(|b| *b == b'\n')
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].len(), 50, "line 1: ordinary");
        assert_eq!(lines[1].len(), 1800, "line 2: mega, fits");
        assert_eq!(lines[2].len(), 50, "line 3: ordinary");
    }
    #[test]
    fn usize_ceiling_preserves_every_value_representable_on_this_platform() {
        // this dev platform's usize is 64-bit, so try_from(u64) always
        // succeeds here and the saturating fallback is unreachable on THIS
        // machine — the regression this function exists for (a bare
        // `as usize` silently truncating a >4GiB target_bytes on a 32-bit
        // target) can only be observed on a genuinely 32-bit build. this
        // pins the platform-independent half of the contract: no value
        // representable in usize is ever altered.
        assert_eq!(crate::usize_ceiling(0), 0);
        assert_eq!(crate::usize_ceiling(12_345), 12_345);
        assert_eq!(crate::usize_ceiling(u64::MAX), usize::MAX);
    }
}

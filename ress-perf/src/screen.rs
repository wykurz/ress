// vt-emulator screen model: a thin wrapper over vt100::Parser, chosen (see the task report)
// over avt and termwiz/wezterm-term for the only criterion that's a hard functional
// requirement, not a preference — vt100's Parser::process(&[u8]) is byte-oriented, built on
// the vte crate's own partial-utf8 carry-over across calls, matching read_available's
// non-blocking, arbitrarily-chunked reads exactly. avt's public api (feed_str(&str)/feed(char))
// would require re-deriving that carry-over by hand; wezterm-term (the piece with an actual
// feed-bytes pipeline) isn't published to crates.io standalone, and termwiz's own Surface only
// accepts pre-interpreted Change values, not raw escape-sequence bytes.
//
// runner.rs's poll loop is the live caller (feed, row_text, contents,
// contents_nonblank), reconstructing frames on the measurement path.

/// An in-memory reconstruction of a pty subject's screen, built by feeding it raw output bytes.
pub struct Screen {
    parser: vt100::Parser,
    // found in PR #44 pass 5 (C1): bumped whenever row 0's own content differs across a `feed`
    // call boundary -- see `feed`'s own doc comment for the full derivation and its honest
    // granularity limit.
    top_row_generation: u64,
}

impl Screen {
    /// Creates a blank screen sized `rows` by `cols`. No scrollback is kept — only the current
    /// visible screen matters for perf.sh's settled row-content detection semantics.
    pub fn new(rows: u16, cols: u16) -> Screen {
        Screen {
            parser: vt100::Parser::new(rows, cols, 0),
            top_row_generation: 0,
        }
    }

    /// Feeds raw subject output through the vt emulator, updating the reconstructed screen.
    /// `bytes` may end mid-way through a multi-byte utf-8 sequence — vte carries the partial
    /// sequence to the next `feed` call rather than corrupting or dropping it.
    ///
    /// found in PR #44 pass 5 (C1): also bumps `top_row_generation` (see its own accessor
    /// below) whenever row 0's own content differs from what it was immediately before this
    /// call.
    ///
    /// Honesty note, stated directly rather than left implicit: this can only ever see a change
    /// that survives to a `feed` CALL BOUNDARY -- the harness's own actual observation
    /// granularity, set by however many bytes one `read_available` call happened to return, not
    /// a promise of byte-level or even escape-sequence-level sight into everything the subject
    /// ever wrote. A genuine flicker that both happens AND reverts entirely within the span of
    /// bytes a single `feed` call processes is invisible here -- exactly as it would be to ANY
    /// harness whose only view of the subject is its own drained-read boundaries, not a
    /// limitation unique to this mechanism. Conversely, a read boundary that happens to land
    /// mid-way through the subject's own row-0 rewrite (so this call's own `bytes` leave row 0
    /// genuinely half-updated when this function returns) is a REAL, actually-observed
    /// intermediate state -- bumping the generation for it is honest, not a false alarm, even
    /// though a differently-chunked read might never have caught it.
    pub fn feed(&mut self, bytes: &[u8]) {
        let before = self.row_text(0);
        self.parser.process(bytes);
        if self.row_text(0) != before {
            self.top_row_generation += 1;
        }
    }

    /// How many times `feed` has observed row 0's own content differ across one of its own call
    /// boundaries, since this `Screen` was created. See `feed`'s own doc comment for exactly
    /// what this can and cannot see -- the granularity is the read-drain boundary, not a
    /// continuous or byte-level watch.
    pub fn top_row_generation(&self) -> u64 {
        self.top_row_generation
    }

    /// Returns `row`'s plain-text contents, trailing spaces trimmed. Out-of-range rows return
    /// an empty string (the screen was constructed with a fixed row count).
    pub fn row_text(&self, row: u16) -> String {
        let (_, cols) = self.parser.screen().size();
        self.parser
            .screen()
            .rows(0, cols)
            .nth(usize::from(row))
            .unwrap_or_default()
            .trim_end_matches(' ')
            .to_string()
    }

    /// Returns the whole screen's plain-text contents (every row, newline-joined) -- for a
    /// predicate that looks for a needle anywhere on screen rather than on one specific row
    /// (e.g. jump-to-end, which lands its target at the bottom of the viewport, not the top).
    pub fn contents(&self) -> String {
        self.parser.screen().contents()
    }
}

#[cfg(test)]
mod tests {
    use crate::test_support::{TempFile, on_path, sibling_bin_path};

    // captures whatever a still-running interactive subject (ress, less) paints within
    // `duration` — unlike pty.rs's drain_to_eof, there's no eof to wait for: the subject stays
    // alive after its first paint, so the capture window is time-bounded instead.
    fn capture_for(child: &mut crate::pty::PtyChild, duration: std::time::Duration) -> Vec<u8> {
        let mut out = Vec::new();
        let deadline = std::time::Instant::now() + duration;
        while std::time::Instant::now() < deadline {
            match child.read_available(&mut out).unwrap() {
                crate::pty::ReadStatus::Data(_) => {}
                crate::pty::ReadStatus::Eof => break,
                crate::pty::ReadStatus::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            }
        }
        out
    }

    // a fixture in ress-filegen's own format (see ress-filegen's fill_line: `{line_no:010} `) —
    // hand-written here rather than depending on ress-filegen, since only the 10-digit prefix
    // that row_text is asked to find actually matters to this test.
    //
    // process id alone is not a unique key: under plain `cargo test` (not nextest -- notably
    // the nix package's own check phase), every test in this binary shares ONE process and
    // runs on a thread pool, so the two real-pager tests below -- both calling this helper --
    // would collide on the identical path and race each other's TempFile drop/unlink against
    // the other's still-reading subprocess. An atomic per-call counter, same pattern as
    // runner.rs's `FakeHome::create`/`NEXT_FAKE_HOME_ID`, makes every call's path unique
    // regardless of how many tests (or threads) call it concurrently.
    static NEXT_FIXTURE_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    fn write_fixture() -> (TempFile, std::path::PathBuf) {
        let id = NEXT_FIXTURE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("ress-perf-screen-{}-{id}.txt", std::process::id()));
        let mut contents = String::new();
        for n in 1..=40u32 {
            contents.push_str(&format!(
                "{n:010} the quick brown fox jumps over the lazy dog\n"
            ));
        }
        std::fs::write(&path, contents).unwrap();
        (TempFile(path.clone()), path)
    }

    #[test]
    fn real_ress_frame_reveals_the_fixtures_line_number_prefix() {
        let (_guard, fixture) = write_fixture();
        let argv = vec![
            sibling_bin_path("ress").into(),
            std::ffi::OsString::from(&fixture),
        ];
        let env = [
            ("TERM".to_string(), "tmux-256color".to_string()),
            ("LC_ALL".to_string(), "C.utf8".to_string()),
        ];
        let mut child = crate::pty::PtyChild::spawn(&argv, &env, (24, 80)).unwrap();
        let captured = capture_for(&mut child, std::time::Duration::from_secs(2));
        assert!(!captured.is_empty(), "expected ress to paint something");
        let mut screen = super::Screen::new(24, 80);
        screen.feed(&captured);
        assert!(
            screen.row_text(0).contains("0000000001"),
            "row 0 missing the fixture's line-number prefix: {:?}",
            screen.row_text(0)
        );
    }

    #[test]
    fn real_less_dash_s_frame_reveals_the_fixtures_line_number_prefix() {
        let (_guard, fixture) = write_fixture();
        let argv = vec![
            on_path("less").into(),
            std::ffi::OsString::from("-S"),
            std::ffi::OsString::from(&fixture),
        ];
        let env = [
            ("TERM".to_string(), "tmux-256color".to_string()),
            ("LC_ALL".to_string(), "C.utf8".to_string()),
            ("LESSHISTFILE".to_string(), "-".to_string()),
        ];
        let mut child = crate::pty::PtyChild::spawn(&argv, &env, (24, 80)).unwrap();
        let captured = capture_for(&mut child, std::time::Duration::from_secs(2));
        assert!(!captured.is_empty(), "expected less -S to paint something");
        let mut screen = super::Screen::new(24, 80);
        screen.feed(&captured);
        assert!(
            screen.row_text(0).contains("0000000001"),
            "row 0 missing the fixture's line-number prefix: {:?}",
            screen.row_text(0)
        );
    }

    #[test]
    fn alt_screen_enter_clear_and_cursor_moves_reconstruct_rows() {
        let mut screen = super::Screen::new(5, 20);
        // enter alt screen, clear it, home the cursor, write a line, then jump to row 3 col 5
        // (1-indexed CSI addressing) and write another — the same shape both ress and less use
        // for their first paint (see the vt-crate evaluation's captured real frames).
        screen.feed(b"\x1b[?1049h\x1b[2J\x1b[1;1HHello\x1b[3;5HWorld");
        assert_eq!(screen.row_text(0), "Hello");
        assert_eq!(screen.row_text(1), "");
        assert_eq!(screen.row_text(2), "    World");
    }

    #[test]
    fn row_zero_is_blank_until_painted() {
        let mut screen = super::Screen::new(5, 20);
        assert!(
            screen.row_text(0).is_empty(),
            "a fresh screen should be blank"
        );
        screen.feed(b"\x1b[?1049h\x1b[2J\x1b[1;1Hpainted");
        assert_eq!(screen.row_text(0), "painted");
    }

    // found in PR #44 pass 5 (C1): pins `top_row_generation`'s own basic contract directly.
    #[test]
    fn top_row_generation_starts_at_zero_on_a_fresh_screen() {
        let screen = super::Screen::new(5, 20);
        assert_eq!(screen.top_row_generation(), 0);
    }

    #[test]
    fn top_row_generation_bumps_when_row_zero_changes() {
        let mut screen = super::Screen::new(5, 20);
        screen.feed(b"\x1b[?1049h\x1b[2J\x1b[1;1Hfirst");
        assert_eq!(
            screen.top_row_generation(),
            1,
            "the first paint is a change"
        );
        screen.feed(b"\x1b[1;1H\x1b[2Ksecond");
        assert_eq!(
            screen.top_row_generation(),
            2,
            "overwriting row 0 with different content is a second change"
        );
    }

    // the honesty property `feed`'s own doc comment states directly: writes to any OTHER row
    // never bump this counter, only row 0's own content crossing a feed-call boundary does.
    #[test]
    fn top_row_generation_does_not_bump_when_only_another_row_changes() {
        let mut screen = super::Screen::new(5, 20);
        screen.feed(b"\x1b[?1049h\x1b[2J\x1b[1;1Hsteady");
        assert_eq!(screen.top_row_generation(), 1);
        screen.feed(b"\x1b[3;1Hunrelated-row-two-activity");
        assert_eq!(
            screen.top_row_generation(),
            1,
            "row 0 never changed -- a write to a different row must not bump this counter"
        );
    }

    // re-feeding the IDENTICAL content (a no-op redraw, or a feed call carrying bytes that
    // don't touch row 0 at all) must not bump the counter either -- it tracks CONTENT changes
    // across the boundary, not merely "a feed call happened."
    #[test]
    fn top_row_generation_does_not_bump_when_the_content_is_unchanged() {
        let mut screen = super::Screen::new(5, 20);
        screen.feed(b"\x1b[?1049h\x1b[2J\x1b[1;1Hsame");
        assert_eq!(screen.top_row_generation(), 1);
        screen.feed(b"\x1b[1;1Hsame");
        assert_eq!(
            screen.top_row_generation(),
            1,
            "re-writing the identical content is not an observed change"
        );
        screen.feed(b"");
        assert_eq!(
            screen.top_row_generation(),
            1,
            "an empty feed call touches nothing"
        );
    }
}

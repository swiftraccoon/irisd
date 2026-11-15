//! The rules that read words: timeless wording and counts of cameras, over
//! prose whose sentences wrap from line to line.

use super::text;

/// The forms of the words `STANDARDS.md` §7 bars that place a statement at
/// the time it was written. `new`, `old`, and `future` are barred where they
/// say when (`the new index`, `future work`) and pass where they say which
/// (`a new run starts`, `once a file is this old`, `the session's future`).
pub const TIMELESS: [&str; 16] = [
    "currently",
    "presently",
    "eventually",
    "latest",
    "existing",
    "soon",
    "now",
    "the new",
    "the old",
    "in the future",
    "future work",
    "future version",
    "future versions",
    "future release",
    "as of this writing",
    "does not yet",
];

/// Phrases that hold a barred word and say nothing of when the text was
/// written.
const IDIOMS: [&str; 4] = ["as soon as", "now and then", "from now on", "of now and"];

/// The words that count.
const NUMBERS: [&str; 20] = [
    "both", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten", "eleven",
    "twelve", "thirteen", "fourteen", "fifteen", "sixteen", "twenty", "thirty", "forty", "dozen",
];

/// The words that, between a number and *cameras*, make the cameras a site's.
const SITE_WORDS: [&str; 3] = ["test", "live", "real"];

/// The longest quotation that is read as one: an unpaired quote mark does not
/// hide the rest of a paragraph.
const QUOTE_MAX: usize = 400;

/// A stretch of prose as one line of lowercase words a space apart, without
/// its code spans and addresses, and the line each word came from.
#[derive(Debug, Default)]
struct Block {
    /// The words.
    words: String,
    /// The offset in `words` at which each source line's words start.
    starts: Vec<(usize, usize)>,
}

impl Block {
    /// The block of `lines`, each with its number.
    fn new(lines: &[(usize, &str)]) -> Self {
        let mut block = Self::default();
        for (number, line) in lines {
            block.starts.push((block.words.len(), *number));
            let prose = text::without_code_spans(line).to_lowercase();
            for word in prose
                .split_whitespace()
                .filter(|word| !word.contains("://"))
            {
                block.words.extend(word.chars().map(|ch| match ch {
                    '“' | '”' => '"',
                    other => other,
                }));
                block.words.push(' ');
            }
        }
        block
    }

    /// The source line of the word at `offset`.
    fn line_at(&self, offset: usize) -> usize {
        let after = self.starts.partition_point(|(start, _)| *start <= offset);
        self.starts
            .get(after.saturating_sub(1))
            .map_or(0, |(_, line)| *line)
    }

    /// The words with what stands between paired quote marks blanked, and
    /// with the idioms blanked: neither is the document speaking of its time.
    fn own_words(&self) -> String {
        let mut bytes = self.words.clone().into_bytes();
        let mut at = 0;
        while let Some(open) = bytes[at..]
            .iter()
            .position(|byte| *byte == b'"')
            .map(|n| at + n)
        {
            let reach = (open + 1 + QUOTE_MAX).min(bytes.len());
            match bytes[open + 1..reach].iter().position(|byte| *byte == b'"') {
                Some(length) => {
                    bytes[open + 1..open + 1 + length].fill(b' ');
                    at = open + length + 2;
                }
                None => at = open + 1,
            }
        }
        let mut words = String::from_utf8_lossy(&bytes).into_owned();
        for idiom in IDIOMS {
            words = words.replace(idiom, &" ".repeat(idiom.len()));
        }
        words
    }
}

/// Phrases that tell how a text or a change came to be in place of what it
/// is. A commit's body gives the reason for the change and the behaviour it
/// brings (`STANDARDS.md` §9), and a committed file says nothing of what
/// anyone did or decided (`AGENTS.md` rule 9): no other commit, no abandoned
/// attempt, no review, nobody finding or deciding.
pub const NARRATION: [&str; 26] = [
    "this commit",
    "the next commit",
    "the previous commit",
    "an earlier commit",
    "earlier commits",
    "as one commit",
    "squashed",
    "withdrawn",
    "turned out",
    "was tried",
    "were tried",
    "was considered",
    "were considered",
    "was given up",
    "the first design",
    "the first attempt",
    "we found",
    "we decided",
    "we tried",
    "it was found",
    "found by hand",
    "audit by hand",
    "audits by hand",
    "each audit",
    "a fuzz run found",
    "a reviewer accepted",
];

/// The phrases of `terms` in `lines`, a stretch of prose with no blank line
/// in it: the line each stands on and the phrase.
fn found_in(lines: &[(usize, &str)], terms: &[&'static str]) -> Vec<(usize, &'static str)> {
    let block = Block::new(lines);
    let words = block.own_words();
    let mut found: Vec<(usize, &'static str)> = Vec::new();
    for term in terms {
        for offset in text::word_offsets(&words, term) {
            found.push((block.line_at(offset), *term));
        }
    }
    found.sort_unstable();
    found.dedup();
    found
}

/// The timeless words in `lines`: the line each stands on and the word.
pub fn timeless(lines: &[(usize, &str)]) -> Vec<(usize, &'static str)> {
    found_in(lines, &TIMELESS)
}

/// The phrases of [`NARRATION`] in `lines`: the line each stands on and the
/// phrase.
pub fn narration(lines: &[(usize, &str)]) -> Vec<(usize, &'static str)> {
    found_in(lines, &NARRATION)
}

/// The lines of `lines` that count a site's cameras: a number, then
/// *cameras* at once or after *test*, *live*, or *real*, or then *camera
/// streams* or *models*, which count them as well. Cameras a benchmark
/// replays or generates are a load, and a number that is a limit of the
/// software says so in other words.
pub fn camera_counts(lines: &[(usize, &str)]) -> Vec<usize> {
    let block = Block::new(lines);
    let mut found = Vec::new();
    let mut offset = 0;
    let words: Vec<(usize, &str)> = block
        .words
        .split(' ')
        .map(|word| {
            let at = offset;
            offset += word.len() + 1;
            (at, word)
        })
        .collect();
    let bare = |word: &str| -> String {
        word.trim_matches(|ch: char| !ch.is_alphanumeric())
            .to_owned()
    };
    for (index, (at, word)) in words.iter().enumerate() {
        let number = word.trim_start_matches('(');
        let counts = NUMBERS.contains(&number)
            || (!number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit()));
        let next = |step: usize| words.get(index + step).map(|(_, word)| bare(word));
        let cameras = next(1).as_deref() == Some("cameras")
            || (next(1).is_some_and(|word| SITE_WORDS.contains(&word.as_str()))
                && next(2).as_deref() == Some("cameras"));
        // The streams of a site's cameras and the models among them count
        // its cameras as well.
        let streams = next(1).as_deref() == Some("camera") && next(2).as_deref() == Some("streams");
        let models = next(1).as_deref() == Some("models");
        if counts && (cameras || streams || models) {
            found.push(block.line_at(*at));
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str) -> Vec<(usize, &'static str)> {
        let lines: Vec<(usize, &str)> = text.lines().enumerate().collect();
        timeless(&lines)
    }

    #[test]
    fn words_that_date_a_statement_are_found() {
        assert_eq!(
            words("The index is now built; the new one is the latest."),
            [(0, "latest"), (0, "now"), (0, "the new")]
        );
        assert_eq!(
            words("It does not\nyet work, as of this\nwriting."),
            [(0, "does not yet"), (1, "as of this writing")]
        );
        assert_eq!(
            words("Future work: an existing file."),
            [(0, "existing"), (0, "future work")]
        );
    }

    #[test]
    fn words_that_say_which_and_not_when_pass() {
        let passing = [
            "A new run starts once a file is this old.",
            "The session's future ends, then a new init segment follows.",
            "Published as soon as it is pushed; it stalls now and then.",
            "Frames pushed from now on, to the earlier of now and the grace.",
            "wraps as\nsoon as\nit can",
            "Call `now()` and see https://example.com/latest/now.html for more.",
            "The manual says \"does not yet implement\" and “currently”.",
            "The renewal threshold is knowable.",
        ];
        for text in passing {
            assert_eq!(words(text), [], "{text}");
        }
        // A quote mark without its pair hides nothing.
        assert_eq!(words("a 5\" disk is currently used"), [(0, "currently")]);
    }

    #[test]
    fn the_story_of_a_change_is_found_and_its_reason_is_not() {
        let told = |text: &str| -> Vec<&'static str> {
            let lines: Vec<(usize, &str)> = text.lines().enumerate().collect();
            narration(&lines)
                .into_iter()
                .map(|(_, phrase)| phrase)
                .collect()
        };
        assert_eq!(
            told("The rule was built and then withdrawn."),
            ["withdrawn"]
        );
        assert_eq!(
            told("Each audit found it; this commit fixes it."),
            ["each audit", "this commit"]
        );
        assert_eq!(
            told("It turned out that\nthe first design was wrong, so we\ndecided otherwise."),
            ["turned out", "the first design", "we decided"]
        );
        assert_eq!(
            told("A fuzz run found the difference, and I fixed it."),
            ["a fuzz run found"]
        );
        let passing = [
            "A run ends when one frame's timestamp is half a second from the count.",
            "The rewrite refuses a carriage return inside a line, because it would end the line.",
            "Run `cargo audit`; the supply-chain audit covers every lockfile.",
            "The viewer plays (i) the picture and (ii) the sound; see section I.",
        ];
        for text in passing {
            assert_eq!(told(text), Vec::<&str>::new(), "{text}");
        }
    }

    #[test]
    fn a_count_of_a_sites_cameras_is_found() {
        let counted = |text: &str| -> Vec<usize> {
            let lines: Vec<(usize, &str)> = text.lines().enumerate().collect();
            camera_counts(&lines)
        };
        assert_eq!(counted("ran on six cameras"), [0]);
        assert_eq!(counted("(six and seven live cameras)"), [0]);
        assert_eq!(counted("with 14\ntest cameras."), [0]);
        assert_eq!(counted("a soak of 14 camera streams"), [0]);
        assert_eq!(counted("recording fourteen camera streams at"), [0]);
        assert_eq!(counted("test cameras of two models"), [0]);
        let passing = [
            "6. The test cameras are live devices.",
            "(default 256, the most cameras a run drives)",
            "at most 64 distinct cameras are listed",
            "14 replayed cameras at a stated bit rate",
            "issue #322 cameras that send",
            "three sessions per camera",
            "a camera of another model",
            "two streams of a camera",
        ];
        for text in passing {
            assert_eq!(counted(text), Vec::<usize>::new(), "{text}");
        }
    }
}

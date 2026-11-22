//! The rules that read words, over prose whose sentences wrap from line to
//! line. They find timeless wording, counts of cameras, long sentences,
//! barred phrases, the crate called by another name, and possessives on
//! code.

use super::finding::{Finding, Rule};
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

/// The most words a sentence holds.
pub const SENTENCE_MAX: usize = 35;

/// The most words a quotation holds and still counts in its sentence. A
/// longer one quotes a published source, whose sentences are not the
/// document's to shorten.
const QUOTED_MAX: usize = 10;

/// The marks that may stand before the first letter of a sentence.
const OPENERS: [char; 10] = ['(', '[', '{', '<', '"', '\'', '“', '‘', '*', '_'];

/// The marks that may stand after the stop of a sentence.
const CLOSERS: [char; 9] = [')', ']', '}', '"', '\'', '”', '’', '*', '_'];

/// Words that end in a period and never end a sentence.
const ABBREVIATIONS: [&str; 4] = ["e.g.", "i.e.", "cf.", "vs."];

/// Words that end in a period and end no sentence before a lowercase word,
/// though they may end one before a capital.
const LOWERCASE_ABBREVIATIONS: [&str; 10] = [
    "etc.", "al.", "a.k.a.", "approx.", "incl.", "resp.", "viz.", "no.", "fig.", "p.",
];

/// What a line of prose is to the rule of sentence length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Line<'a> {
    /// Text that goes on with the paragraph or list item before it.
    Text(&'a str),
    /// The first line of a list item, without its marker.
    Item(&'a str),
    /// A row of a table.
    Row(&'a str),
    /// A heading or a link's definition, which holds no sentence.
    Apart,
}

/// What `line` is, without its indentation and its quote marks.
fn line_kind(line: &str) -> Line<'_> {
    let rest = line.trim_start().trim_start_matches(['>', ' ']).trim_end();
    if rest.starts_with('|') {
        return Line::Row(rest);
    }
    let hashes = rest.bytes().take_while(|byte| *byte == b'#').count();
    let heading = hashes > 0 && rest.as_bytes().get(hashes).is_none_or(|byte| *byte == b' ');
    if heading || (rest.starts_with('[') && rest.contains("]: ")) {
        return Line::Apart;
    }
    for marker in ["- ", "* ", "+ "] {
        if let Some(item) = rest.strip_prefix(marker) {
            return Line::Item(item);
        }
    }
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    let numbered = rest[digits..]
        .strip_prefix(". ")
        .or_else(|| rest[digits..].strip_prefix(") "));
    match numbered {
        Some(item) if digits > 0 => Line::Item(item),
        _ => Line::Text(rest),
    }
}

/// The cells of a table's row: split at each `|` that no code span holds and
/// no backslash escapes. A rule line has none.
fn cells(row: &str) -> Vec<&str> {
    if row.bytes().all(|byte| b"|-: ".contains(&byte)) {
        return Vec::new();
    }
    let bytes = row.as_bytes();
    let mut cells = Vec::new();
    let mut start = 0;
    let mut code = false;
    for (at, byte) in bytes.iter().enumerate() {
        match byte {
            b'`' => code = !code,
            b'|' if !code && (at == 0 || bytes[at - 1] != b'\\') => {
                cells.push(&row[start..at]);
                start = at + 1;
            }
            _ => {}
        }
    }
    cells.push(&row[start..]);
    cells
        .into_iter()
        .map(str::trim)
        .filter(|cell| !cell.is_empty())
        .collect()
}

/// The stretches of `lines` that no sentence crosses: each paragraph, list
/// item, and table cell. Headings and link definitions are left out.
fn units<'a>(lines: &[(usize, &'a str)]) -> Vec<Vec<(usize, &'a str)>> {
    let mut units: Vec<Vec<(usize, &str)>> = vec![Vec::new()];
    for (number, line) in lines {
        match line_kind(line) {
            Line::Text(text) => units
                .last_mut()
                .expect("one unit at least")
                .push((*number, text)),
            Line::Item(text) => units.push(vec![(*number, text)]),
            Line::Row(row) => {
                units.extend(cells(row).into_iter().map(|cell| vec![(*number, cell)]));
                units.push(Vec::new());
            }
            Line::Apart => units.push(Vec::new()),
        }
    }
    units.retain(|unit| !unit.is_empty());
    units
}

/// The offset of the run of `run` backticks at or after `from` that closes
/// a code span.
pub fn closing_run(bytes: &[u8], from: usize, run: usize) -> Option<usize> {
    let mut at = from;
    while at < bytes.len() {
        if bytes[at] == b'`' {
            let length = bytes[at..].iter().take_while(|byte| **byte == b'`').count();
            if length == run {
                return Some(at);
            }
            at += length;
        } else {
            at += 1;
        }
    }
    None
}

/// A token of prose: a run of text without white space, where a code span is
/// one piece whatever it holds.
#[derive(Debug, Clone, Copy)]
struct Token {
    /// Its first byte in the text.
    start: usize,
    /// The byte after its last.
    end: usize,
    /// Whether it counts as a word: it holds a letter, a digit, or code.
    word: bool,
    /// The quotation that leaves it out of the count, if one does.
    quote: Option<usize>,
}

/// The tokens of `text`, and the offsets of its quote marks outside code.
fn tokens(text: &str) -> (Vec<Token>, Vec<usize>) {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut quotes = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at].is_ascii_whitespace() {
            at += 1;
            continue;
        }
        let start = at;
        let mut word = false;
        while at < bytes.len() && !bytes[at].is_ascii_whitespace() {
            if bytes[at] == b'`' {
                let run = bytes[at..].iter().take_while(|byte| **byte == b'`').count();
                match closing_run(bytes, at + run, run) {
                    Some(close) => {
                        word = true;
                        at = close + run;
                    }
                    None => at += run,
                }
                continue;
            }
            let ch = text[at..]
                .chars()
                .next()
                .expect("a character at a boundary");
            if matches!(ch, '"' | '“' | '”') {
                quotes.push(at);
            }
            word |= ch.is_alphanumeric();
            at += ch.len_utf8();
        }
        tokens.push(Token {
            start,
            end: at,
            word,
            quote: None,
        });
    }
    (tokens, quotes)
}

/// Marks the tokens of each quotation of more than [`QUOTED_MAX`] words. A
/// quote mark pairs with the next one within [`QUOTE_MAX`] bytes; one without
/// such a pair quotes nothing.
fn mark_quotations(tokens: &mut [Token], quotes: &[usize]) {
    let mut index = 0;
    let mut pair = 0;
    while index + 1 < quotes.len() {
        let (open, close) = (quotes[index], quotes[index + 1]);
        if close - open > QUOTE_MAX {
            index += 1;
            continue;
        }
        index += 2;
        pair += 1;
        let first = tokens.partition_point(|token| token.end <= open);
        let after = tokens.partition_point(|token| token.start <= close);
        let inside = &mut tokens[first..after];
        if inside.iter().filter(|token| token.word).count() > QUOTED_MAX {
            for token in inside {
                token.quote = Some(pair);
            }
        }
    }
}

/// Whether `token` ends a sentence that `next` starts: a stop, then a letter,
/// a digit, a section sign, or a code span, perhaps behind a mark. A
/// lowercase letter starts a sentence too, as the name of a program may
/// (`ffmpeg reads it`), but not after an abbreviation or an initial.
fn ends_sentence(token: &str, next: &str) -> bool {
    let stop = token.trim_end_matches(CLOSERS);
    if !stop.ends_with(['.', '?', '!']) {
        return false;
    }
    let bare = stop.trim_start_matches(OPENERS).to_lowercase();
    if ABBREVIATIONS.contains(&bare.as_str()) {
        return false;
    }
    let start = next.trim_start_matches(OPENERS);
    let lowercase = start.starts_with(char::is_lowercase)
        && !LOWERCASE_ABBREVIATIONS.contains(&bare.as_str())
        && bare.chars().count() > 2;
    lowercase
        || start.starts_with(|ch: char| {
            ch.is_uppercase() || ch.is_ascii_digit() || ch == '`' || ch == '§'
        })
}

/// The text of one unit as one line, and the offset in it at which each
/// source line starts, with the line's number.
fn unit_text(unit: &[(usize, &str)]) -> (String, Vec<(usize, usize)>) {
    let mut text = String::new();
    let mut starts = Vec::new();
    for (number, line) in unit {
        starts.push((text.len(), *number));
        text.push_str(line);
        text.push(' ');
    }
    (text, starts)
}

/// Whether the token at `index` of `tokens`, the tokens of `text`, ends a
/// sentence: it is the last, or a stop before the start of another, outside
/// a quotation.
fn ends_at(text: &str, tokens: &[Token], index: usize) -> bool {
    let token = tokens[index];
    let next = tokens.get(index + 1);
    let inside = token.quote.is_some() && next.is_some_and(|next| next.quote == token.quote);
    next.is_none_or(|next| {
        !inside && ends_sentence(&text[token.start..token.end], &text[next.start..next.end])
    })
}

/// The sentences of one unit that hold more than [`SENTENCE_MAX`] words.
fn unit_sentences(unit: &[(usize, &str)]) -> Vec<(usize, usize)> {
    let (text, starts) = unit_text(unit);
    let line_at = |offset: usize| {
        let after = starts.partition_point(|(start, _)| *start <= offset);
        starts[after.saturating_sub(1)].1
    };
    let (mut tokens, quotes) = tokens(&text);
    mark_quotations(&mut tokens, &quotes);
    let mut found = Vec::new();
    let mut count = 0;
    let mut first = None;
    for (index, token) in tokens.iter().enumerate() {
        if token.word && token.quote.is_none() {
            count += 1;
            first.get_or_insert(token.start);
        }
        if ends_at(&text, &tokens, index) {
            if let Some(first) = first.filter(|_| count > SENTENCE_MAX) {
                found.push((line_at(first), count));
            }
            count = 0;
            first = None;
        }
    }
    found
}

/// The sentences of `lines`, a stretch of prose with no blank line in it,
/// that hold more than [`SENTENCE_MAX`] words: the line each starts on and
/// its count. A sentence ends at a stop before the start of another, as
/// [`ends_sentence`] reads it, and at the end of its paragraph, list item, or
/// table cell; a colon or a semicolon ends none. A code span and a URL count
/// as one word each, and a mark alone as none. A quotation of more than
/// [`QUOTED_MAX`] words is not counted.
fn long_sentences(lines: &[(usize, &str)]) -> Vec<(usize, usize)> {
    units(lines)
        .iter()
        .flat_map(|unit| unit_sentences(unit))
        .collect()
}

/// The sentences of `lines`, a stretch of prose with no blank line in it,
/// each as one line of text. They end where [`long_sentences`] ends them.
pub fn sentences(lines: &[(usize, &str)]) -> Vec<String> {
    let mut found = Vec::new();
    for unit in units(lines) {
        let (text, _) = unit_text(&unit);
        let (mut tokens, quotes) = tokens(&text);
        mark_quotations(&mut tokens, &quotes);
        let mut first = None;
        for (index, token) in tokens.iter().enumerate() {
            let start = *first.get_or_insert(token.start);
            if ends_at(&text, &tokens, index) {
                found.push(text[start..token.end].to_owned());
                first = None;
            }
        }
    }
    found
}

/// Comparatives that follow `reads as` in plain English: `reads as many
/// bytes as it can`.
const COMPARATIVES: [&str; 10] = [
    "many", "much", "few", "little", "far", "long", "often", "soon", "fast", "well",
];

/// Words before `classes` that make it a noun: `the two classes each hold`.
const DETERMINERS: [&str; 12] = [
    "the", "both", "two", "three", "these", "those", "all", "its", "their", "our", "some", "many",
];

/// Words before `of its own` that make `its own` a pronoun and no filler:
/// `in place of its own`, `instead of its own`.
const PRONOUN_BEFORE: [&str; 2] = ["place", "instead"];

/// The advice for `reads as` and for `read as` before a quotation or code.
const READS_AS: &str = "write \"has the text\" or \"is treated as\"";

/// The barred phrases matched word for word, and what to write in their
/// place.
const BARRED: [(&[&str], &str); 10] = [
    (&["reads", "as"], READS_AS),
    (&["is", "asked", "of"], "say who does what"),
    (&["are", "asked", "of"], "say who does what"),
    (&["asked", "of", "the"], "say who does what"),
    (&["of", "its", "own"], "write \"a separate …\", or drop it"),
    (&["of", "their", "own"], "write \"separate …\", or drop it"),
    (&["head", "stage"], "name the step"),
    (&["classes", "each"], "write \"classifies\""),
    (&["classed"], "write \"classified\""),
    (&["classing"], "write \"classifying\""),
];

/// The verbs of `holds each answer to its rule`.
const HOLD: [&str; 3] = ["holds", "hold", "held"];

/// What may follow `to` after a form of `hold`.
const HELD_TO: [&str; 4] = ["that", "it", "them", "its"];

/// The most words that may stand between a form of `hold` and its `to`,
/// counting the `to`.
const HOLD_REACH: usize = 6;

/// The words of a stretch of prose, with the text of its code spans and of
/// its quotations blanked and their marks kept, so that what follows a word
/// shows. Letter case is kept, and URLs are left out.
#[derive(Debug, Default)]
struct Words {
    /// The prose as one line.
    text: String,
    /// Each word: the line it came from, and where it starts and ends.
    words: Vec<(usize, usize, usize)>,
}

impl Words {
    /// The words of `lines`.
    fn new(lines: &[(usize, &str)]) -> Self {
        let mut joined = String::new();
        let mut starts = Vec::new();
        for (number, line) in lines {
            starts.push((joined.len(), *number));
            joined.push_str(line);
            joined.push(' ');
        }
        // A code span may wrap from one line to the next.
        let mut bytes = joined.into_bytes();
        let mut at = 0;
        while at < bytes.len() {
            if bytes[at] != b'`' {
                at += 1;
                continue;
            }
            let run = bytes[at..].iter().take_while(|byte| **byte == b'`').count();
            match closing_run(&bytes, at + run, run) {
                Some(close) => {
                    bytes[at + run..close].fill(b' ');
                    at = close + run;
                }
                None => at += run,
            }
        }
        let text = String::from_utf8(bytes).expect("whole characters blanked");
        let mut marks: Vec<(usize, usize)> = text
            .match_indices(['"', '“', '”'])
            .map(|(at, mark)| (at, at + mark.len()))
            .collect();
        marks.reverse();
        let mut blank = Vec::new();
        while let Some((open, inside)) = marks.pop() {
            if let Some(&(close, _)) = marks.last().filter(|(close, _)| close - open <= QUOTE_MAX) {
                marks.pop();
                blank.push(inside..close);
            }
        }
        let mut bytes = text.into_bytes();
        for range in blank {
            bytes[range].fill(b' ');
        }
        let text = String::from_utf8(bytes).expect("whole characters blanked");
        let mut words = Vec::new();
        let mut at = 0;
        for word in text.split(' ') {
            if !word.is_empty() && !word.contains("://") {
                let line = starts[starts.partition_point(|(start, _)| *start <= at) - 1].1;
                words.push((line, at, at + word.len()));
            }
            at += word.len() + 1;
        }
        Self { text, words }
    }

    /// The word at `index`.
    fn word(&self, index: usize) -> Option<&str> {
        let (_, start, end) = *self.words.get(index)?;
        Some(&self.text[start..end])
    }

    /// The line of the word at `index`.
    fn line(&self, index: usize) -> usize {
        self.words[index].0
    }
}

/// `word` without the marks before it.
fn lead(word: &str) -> &str {
    word.trim_start_matches(|ch: char| !ch.is_alphanumeric())
}

/// `word` without the marks after it.
fn tail(word: &str) -> &str {
    word.trim_end_matches(|ch: char| !ch.is_alphanumeric())
}

/// Whether `word`, without the marks after it, is one of `list`, in any
/// letter case.
fn one_of(word: Option<&str>, list: &[&str]) -> bool {
    word.is_some_and(|word| {
        list.iter()
            .any(|item| tail(word).eq_ignore_ascii_case(item))
    })
}

/// Whether the words from `at` on are `parts`, in any letter case: marks
/// before the first and after the last, and none between.
fn phrase_at(words: &Words, at: usize, parts: &[&str]) -> bool {
    let last = parts.len() - 1;
    parts.iter().enumerate().all(|(index, part)| {
        let Some(word) = words.word(at + index) else {
            return false;
        };
        let word = if index == 0 { lead(word) } else { word };
        let word = if index == last { tail(word) } else { word };
        word.eq_ignore_ascii_case(part)
    })
}

/// Whether the form of `hold` at `at` is followed, within [`HOLD_REACH`]
/// words of one clause, by `to that`, `to it`, `to them`, or `to its`.
fn held_to(words: &Words, at: usize) -> bool {
    let verb = words.word(at).map_or("", lead);
    if !HOLD.iter().any(|hold| verb.eq_ignore_ascii_case(hold)) {
        return false;
    }
    for step in 1..=HOLD_REACH {
        let Some(word) = words.word(at + step) else {
            return false;
        };
        let object = words.word(at + step + 1);
        let bare = object.is_some_and(|object| lead(object) == object);
        if word.eq_ignore_ascii_case("to") && bare && one_of(object, &HELD_TO) {
            return true;
        }
        if word.ends_with(['.', ';', ':', '?', '!']) {
            return false;
        }
    }
    false
}

/// The barred phrases among `words`: the line each stands on, the phrase,
/// and what to write instead. Each is matched as whole words in any letter
/// case, outside code spans and quotations.
fn barred(words: &Words) -> Vec<(usize, String, &'static str)> {
    let mut found = Vec::new();
    let mut covered = 0;
    for index in 0..words.words.len() {
        // Every phrase starts with one of these letters: most words pass here.
        let first = words.word(index).map_or("", lead).bytes().next();
        let starts = first.is_some_and(|byte| b"achior".contains(&byte.to_ascii_lowercase()));
        if index < covered || !starts {
            continue;
        }
        let matched = BARRED.iter().find(|(parts, _)| {
            phrase_at(words, index, parts)
                && !(parts[0] == "reads" && one_of(words.word(index + 2), &COMPARATIVES))
                && !(parts[0] == "classes"
                    && index > 0
                    && one_of(words.word(index - 1).map(lead), &DETERMINERS))
                && !(parts[0] == "of"
                    && index > 0
                    && one_of(words.word(index - 1).map(lead), &PRONOUN_BEFORE))
        });
        let quoted = words
            .word(index + 2)
            .is_some_and(|next| next.starts_with(['"', '“', '`']) || next.starts_with("[`"));
        let (phrase, advice, length) = if let Some((parts, advice)) = matched {
            (parts.join(" "), *advice, parts.len())
        } else if phrase_at(words, index, &["read", "as"]) && quoted {
            ("read as".to_owned(), READS_AS, 2)
        } else if held_to(words, index) {
            let verb = words.word(index).map_or("", lead).to_lowercase();
            (
                format!("{verb} ... to"),
                "say what is checked against what",
                1,
            )
        } else {
            continue;
        };
        covered = index + length;
        found.push((words.line(index), phrase, advice));
    }
    found
}

/// The lines where `the core` names a crate among `words`. The two words
/// count in any letter case outside code spans and quotations, unless a
/// hyphen, a closing parenthesis, `of`, `types`, or a word with a capital
/// follows them. The documents name the crate, `iris-client`. What the rule
/// leaves, such as `the core API` and `(the core)`, can stand for another
/// crate or for none.
fn the_core(words: &Words) -> Vec<usize> {
    let mut found = Vec::new();
    for index in 1..words.words.len() {
        let (Some(the), Some(core)) = (words.word(index - 1), words.word(index)) else {
            continue;
        };
        let after = core
            .get(..4)
            .filter(|head| head.eq_ignore_ascii_case("core"))
            .map(|_| &core[4..]);
        let named = lead(the).eq_ignore_ascii_case("the")
            && after.is_some_and(|after| {
                !after.starts_with(|ch: char| ch.is_alphanumeric() || ch == '-' || ch == ')')
            });
        let next = words.word(index + 1);
        let qualified = after == Some("")
            && (one_of(next, &["of", "types"])
                || next.is_some_and(|next| next.starts_with(char::is_uppercase)));
        if named && !qualified {
            found.push(words.line(index - 1));
        }
    }
    found
}

/// The lines of `lines` where a code span or an intra-doc link takes `'s`:
/// `` `Feed`'s ``, `` [`Feed`]'s ``, or `[Feed]'s`, outside code spans. A
/// link whose label is words, such as `[RFC 8216]'s`, is not code.
fn possessives(lines: &[(usize, &str)]) -> Vec<usize> {
    let possessive = |rest: &str| {
        rest.strip_prefix("'s")
            .or_else(|| rest.strip_prefix("’s"))
            .is_some_and(|after| !after.starts_with(char::is_alphanumeric))
    };
    // A code span may wrap from one line to the next.
    let mut text = String::new();
    let mut starts = Vec::new();
    for (number, line) in lines {
        starts.push((text.len(), *number));
        text.push_str(line);
        text.push(' ');
    }
    let line_at =
        |offset: usize| starts[starts.partition_point(|(start, _)| *start <= offset) - 1].1;
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'`' => {
                let run = bytes[at..].iter().take_while(|byte| **byte == b'`').count();
                let Some(close) = closing_run(bytes, at + run, run) else {
                    at += run;
                    continue;
                };
                at = close + run;
                let rest = &text[at..];
                if possessive(rest) || (rest.starts_with(']') && possessive(&rest[1..])) {
                    found.push(line_at(at));
                }
            }
            b']' => {
                let label = text[..at].rfind('[').map(|open| &text[open + 1..at]);
                let code = label.is_some_and(|label| !label.contains(char::is_whitespace));
                if code && !text[..at].ends_with('`') && possessive(&text[at + 1..]) {
                    found.push(line_at(at));
                }
                at += 1;
            }
            _ => at += 1,
        }
    }
    found
}

/// The findings of the rules of sentence length, barred phrases,
/// `the core`, and possessives on code in `lines`, a stretch of the prose of
/// `file` with no blank line in it.
pub fn findings(file: &str, lines: &[(usize, &str)]) -> Vec<Finding> {
    let words = Words::new(lines);
    let mut found = Vec::new();
    for (line, count) in long_sentences(lines) {
        let message = format!("a sentence of {count} words: split it, at {SENTENCE_MAX} at most");
        found.push(Finding::new(Rule::SentenceLength, file, line, message));
    }
    for (line, phrase, advice) in barred(&words) {
        let message = format!("\"{phrase}\": {advice}");
        found.push(Finding::new(Rule::BarredPhrase, file, line, message));
    }
    for line in the_core(&words) {
        let message = "\"the core\": name the crate, `iris-client`";
        found.push(Finding::new(Rule::TheCore, file, line, message));
    }
    for line in possessives(lines) {
        let message = "a possessive on code: write \"the … of `x`\"";
        found.push(Finding::new(Rule::PossessiveCodeSpan, file, line, message));
    }
    found
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

    /// The lines of `text`, numbered from 0.
    fn numbered(text: &str) -> Vec<(usize, &str)> {
        text.lines().enumerate().collect()
    }

    /// `count` words without a stop.
    fn run_of(count: usize) -> String {
        vec!["word"; count].join(" ")
    }

    /// A sentence of `count` words that starts with a capital.
    fn sentence(count: usize) -> String {
        format!("Word {}.", run_of(count - 1))
    }

    fn sentences(text: &str) -> Vec<(usize, usize)> {
        long_sentences(&numbered(text))
    }

    #[test]
    fn a_sentence_holds_at_most_35_words() {
        assert_eq!(sentences(&sentence(35)), []);
        assert_eq!(sentences(&sentence(36)), [(0, 36)]);
    }

    #[test]
    fn a_long_sentence_is_found_on_the_line_it_starts() {
        let text = format!("A short one.\n{}", sentence(36));
        assert_eq!(sentences(&text), [(1, 36)]);
    }

    #[test]
    fn a_stop_before_a_capital_ends_a_sentence() {
        let text = format!("{} {}", sentence(30), sentence(30));
        assert_eq!(sentences(&text), []);
    }

    #[test]
    fn a_stop_before_a_code_span_ends_a_sentence() {
        let text = format!("{} `Feed` {}", sentence(30), run_of(30));
        assert_eq!(sentences(&text), []);
    }

    #[test]
    fn a_stop_before_a_lowercase_word_ends_a_sentence() {
        let text = format!("{} ffmpeg {}", sentence(30), run_of(30));
        assert_eq!(sentences(&text), []);
    }

    #[test]
    fn a_stop_after_an_abbreviation_ends_no_sentence() {
        let text = format!("{} e.g. {}", run_of(20), sentence(20));
        assert_eq!(sentences(&text), [(0, 41)]);
        let text = format!("{} etc. {}", run_of(20), run_of(20));
        assert_eq!(sentences(&text), [(0, 41)]);
    }

    #[test]
    fn a_colon_or_a_semicolon_ends_no_sentence() {
        let text = format!("{}: {}; {}", run_of(12), run_of(12), run_of(12));
        assert_eq!(sentences(&text), [(0, 36)]);
    }

    #[test]
    fn a_list_item_ends_a_sentence() {
        let text = format!("- {}\n- {}\n1. {}", run_of(30), run_of(30), run_of(30));
        assert_eq!(sentences(&text), []);
    }

    #[test]
    fn a_table_cell_ends_a_sentence() {
        let text = format!("| {} | {} |", run_of(30), run_of(30));
        assert_eq!(sentences(&text), []);
    }

    #[test]
    fn a_rule_line_of_a_table_has_no_cells() {
        assert_eq!(cells("|---|:-:|"), Vec::<&str>::new());
        assert_eq!(cells("| a | `b|c` | d \\| e |"), ["a", "`b|c`", "d \\| e"]);
    }

    #[test]
    fn a_heading_holds_no_sentence() {
        let text = format!("# {}\n## {}", run_of(40), run_of(40));
        assert_eq!(sentences(&text), []);
    }

    #[test]
    fn a_link_definition_holds_no_sentence() {
        let text = format!(
            "{}\n[a]: https://example.com/ \"{}\"",
            run_of(30),
            run_of(10)
        );
        assert_eq!(sentences(&text), []);
    }

    #[test]
    fn a_code_span_counts_as_one_word() {
        assert_eq!(sentences(&format!("{} `a b c d e`.", run_of(34))), []);
        let text = format!("{} `a b c d e`.", run_of(35));
        assert_eq!(sentences(&text), [(0, 36)]);
    }

    #[test]
    fn a_url_counts_as_one_word() {
        let text = format!("{} https://example.com/a?b=c.", run_of(35));
        assert_eq!(sentences(&text), [(0, 36)]);
    }

    #[test]
    fn a_mark_alone_counts_as_no_word() {
        let text = format!("{} — {} – {}.", run_of(12), run_of(12), run_of(11));
        assert_eq!(sentences(&text), []);
    }

    #[test]
    fn a_quotation_of_eleven_words_is_not_counted() {
        let text = format!("{} \"{}\" end.", run_of(30), run_of(11));
        assert_eq!(sentences(&text), []);
    }

    #[test]
    fn sentences_are_parted_where_a_long_one_would_end() {
        let lines = numbered("One is here, e.g. now. Two\nthere: `x`.\n- Three");
        assert_eq!(
            super::sentences(&lines),
            ["One is here, e.g. now.", "Two there: `x`.", "Three"]
        );
    }

    #[test]
    fn a_quotation_parts_no_sentence() {
        let quoted = format!("It says \"{}. Then more\" and ends.", run_of(11));
        assert_eq!(super::sentences(&numbered(&quoted)).len(), 1);
    }

    #[test]
    fn a_quotation_of_ten_words_is_counted() {
        let text = format!("{} “{}” end.", run_of(30), run_of(10));
        assert_eq!(sentences(&text), [(0, 41)]);
    }

    fn barred_in(text: &str) -> Vec<String> {
        let words = Words::new(&numbered(text));
        barred(&words)
            .into_iter()
            .map(|(_, phrase, _)| phrase)
            .collect()
    }

    #[test]
    fn reads_as_is_barred() {
        assert_eq!(barred_in("The field reads as a number."), ["reads as"]);
        assert_eq!(barred_in("It READS AS one."), ["reads as"]);
    }

    #[test]
    fn reads_as_before_a_comparative_passes() {
        assert_eq!(barred_in("It reads as many bytes as it can."), [""; 0]);
    }

    #[test]
    fn read_as_before_a_quotation_or_code_is_barred() {
        assert_eq!(barred_in("A zero is read as \"none\"."), ["read as"]);
        assert_eq!(barred_in("A zero is read as `None`."), ["read as"]);
        assert_eq!(barred_in("A zero is read as [`None`]."), ["read as"]);
    }

    #[test]
    fn read_as_before_a_word_passes() {
        assert_eq!(barred_in("The body is read as JSON."), [""; 0]);
    }

    #[test]
    fn asked_of_is_barred() {
        assert_eq!(barred_in("what is asked of a caller"), ["is asked of"]);
        assert_eq!(barred_in("what are asked of a caller"), ["are asked of"]);
        assert_eq!(barred_in("what was asked of the caller"), ["asked of the"]);
        assert_eq!(barred_in("what is asked of the caller"), ["is asked of"]);
    }

    #[test]
    fn of_its_own_is_barred() {
        assert_eq!(barred_in("a thread of its own"), ["of its own"]);
        assert_eq!(barred_in("threads of their own"), ["of their own"]);
    }

    #[test]
    fn of_its_own_as_a_pronoun_passes() {
        assert_eq!(barred_in("an address in place of its own"), [""; 0]);
        assert_eq!(barred_in("the names instead of their own"), [""; 0]);
    }

    #[test]
    fn head_stage_is_barred() {
        assert_eq!(barred_in("The head stage reads it."), ["head stage"]);
    }

    #[test]
    fn the_verb_class_is_barred() {
        assert_eq!(barred_in("It classes each line."), ["classes each"]);
        assert_eq!(barred_in("A line is classed."), ["classed"]);
        assert_eq!(barred_in("Classing it costs."), ["classing"]);
    }

    #[test]
    fn classes_as_a_noun_passes() {
        assert_eq!(barred_in("The two classes each take a lock."), [""; 0]);
    }

    #[test]
    fn holding_a_thing_to_a_rule_is_barred() {
        assert_eq!(
            barred_in("It holds each answer to its rule."),
            ["holds ... to"]
        );
        assert_eq!(barred_in("A run is held to that."), ["held ... to"]);
        assert_eq!(barred_in("Hold them to it."), ["hold ... to"]);
    }

    #[test]
    fn hold_reaches_six_words_for_its_to() {
        let six = "It holds one two three four five to its rule.";
        assert_eq!(barred_in(six), ["holds ... to"]);
        let seven = "It holds one two three four five six to its rule.";
        assert_eq!(barred_in(seven), [""; 0]);
    }

    #[test]
    fn hold_reaches_no_further_than_its_clause() {
        assert_eq!(barred_in("It holds. To that end it waits."), [""; 0]);
        assert_eq!(
            barred_in("It holds the lock; to its end it waits."),
            [""; 0]
        );
    }

    #[test]
    fn a_phrase_matches_whole_words_only() {
        assert_eq!(
            barred_in("It spreads as far, unclassed, at the overhead stage."),
            [""; 0]
        );
    }

    #[test]
    fn a_code_span_that_wraps_hides_its_words() {
        assert_eq!(barred_in("the span `x y\nof its own` ends here"), [""; 0]);
    }

    #[test]
    fn a_phrase_in_code_or_a_quotation_passes() {
        assert_eq!(
            barred_in("Write `reads as` or \"of its own\" in its place."),
            [""; 0]
        );
    }

    fn cores(text: &str) -> Vec<usize> {
        the_core(&Words::new(&numbered(text)))
    }

    #[test]
    fn the_core_for_the_crate_is_found() {
        assert_eq!(cores("the core builds requests"), [0]);
        assert_eq!(cores("The core's decoder"), [0]);
        assert_eq!(cores("It runs over the core."), [0]);
        assert_eq!(cores("It runs over the\ncore, which checks it."), [0]);
    }

    #[test]
    fn the_core_of_a_thing_passes() {
        assert_eq!(cores("the core of the rule"), [0; 0]);
        assert_eq!(cores("the core types"), [0; 0]);
    }

    #[test]
    fn the_core_before_a_hyphen_passes() {
        assert_eq!(cores("from the core-side end"), [0; 0]);
    }

    #[test]
    fn iris_core_in_a_code_span_passes() {
        assert_eq!(cores("the `iris-core` crate, the `core` module"), [0; 0]);
    }

    #[test]
    fn core_inside_a_longer_word_passes() {
        assert_eq!(cores("the cores, the coredump, the hardcore"), [0; 0]);
    }

    #[test]
    fn the_core_before_a_capital_passes() {
        assert_eq!(cores("before crossing the core API"), [0; 0]);
    }

    #[test]
    fn the_core_in_parentheses_passes() {
        assert_eq!(cores("the time model (the core)."), [0; 0]);
    }

    fn possessed(text: &str) -> Vec<usize> {
        possessives(&numbered(text))
    }

    #[test]
    fn a_possessive_on_a_code_span_is_found() {
        assert_eq!(possessed("the `Feed`'s token"), [0]);
        assert_eq!(possessed("the `Feed`’s token"), [0]);
    }

    #[test]
    fn a_possessive_on_a_code_span_that_wraps_is_found() {
        assert_eq!(possessed("the `Feed\nSource`'s token"), [1]);
    }

    #[test]
    fn a_possessive_on_an_intra_doc_link_is_found() {
        assert_eq!(possessed("the [`Feed`]'s token"), [0]);
        assert_eq!(possessed("the [Feed]'s token"), [0]);
    }

    #[test]
    fn a_possessive_inside_a_code_span_passes() {
        assert_eq!(possessed("write `` `Feed`'s `` or `'s`"), [0; 0]);
    }

    #[test]
    fn a_possessive_on_a_link_of_words_passes() {
        assert_eq!(possessed("the [RFC 8216]'s rules"), [0; 0]);
    }

    #[test]
    fn a_word_after_an_apostrophe_past_a_code_span_passes() {
        assert_eq!(possessed("`a`'sb"), [0; 0]);
    }

    #[test]
    fn a_plural_of_a_code_span_passes() {
        assert_eq!(possessed("two `Feed`s, the `Feed` 's"), [0; 0]);
    }
}

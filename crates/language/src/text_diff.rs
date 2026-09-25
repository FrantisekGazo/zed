use crate::{CharClassifier, CharKind, CharScopeContext, LanguageScope, WordDiffStyle};
use anyhow::{Context, anyhow};
use imara_diff::{Algorithm, Diff, InternedInput, Interner, Token, sources::lines};
use std::{fmt::Write, iter, ops::Range, sync::Arc};

const MAX_WORD_DIFF_LEN: usize = 512;
const MAX_WORD_DIFF_LINE_COUNT: usize = 8;
pub const MAX_BLOCK_WORD_DIFF_LEN: usize = 32 * 1024;

/// Computes a diff between two strings, returning a unified diff string.
pub fn unified_diff(old_text: &str, new_text: &str) -> String {
    unified_diff_with_offsets(old_text, new_text, 0, 0)
}

/// Computes a diff between two strings, returning a unified diff string with
/// hunk headers adjusted to reflect the given starting line numbers (zero-indexed).
pub fn unified_diff_with_offsets(
    old_text: &str,
    new_text: &str,
    old_start_line: u32,
    new_start_line: u32,
) -> String {
    unified_diff_with_context(old_text, new_text, old_start_line, new_start_line, 3)
}

/// Computes a diff between two strings, returning a unified diff string with
/// hunk headers adjusted to reflect the given starting line numbers (zero-indexed),
/// and a configurable number of context lines around changes.
pub fn unified_diff_with_context(
    old_text: &str,
    new_text: &str,
    old_start_line: u32,
    new_start_line: u32,
    context_lines: u32,
) -> String {
    // The builder appends its own line terminators, so tokenize without them.
    let mut input = InternedInput::default();
    input.update_before(old_text.lines());
    input.update_after(new_text.lines());
    let diff = Diff::compute(Algorithm::Histogram, &input);
    let mut builder =
        OffsetUnifiedDiffBuilder::new(&input, old_start_line, new_start_line, context_lines);
    for hunk in diff.hunks() {
        builder.process_change(hunk.before, hunk.after);
    }
    builder.finish()
}

/// A unified diff builder that applies line number offsets to hunk headers.
struct OffsetUnifiedDiffBuilder<'a> {
    before: &'a [Token],
    after: &'a [Token],
    interner: &'a Interner<&'a str>,

    pos: u32,
    before_hunk_start: u32,
    after_hunk_start: u32,
    before_hunk_len: u32,
    after_hunk_len: u32,

    old_line_offset: u32,
    new_line_offset: u32,
    context_lines: u32,

    buffer: String,
    dst: String,
}

impl<'a> OffsetUnifiedDiffBuilder<'a> {
    fn new(
        input: &'a InternedInput<&'a str>,
        old_line_offset: u32,
        new_line_offset: u32,
        context_lines: u32,
    ) -> Self {
        Self {
            before_hunk_start: 0,
            after_hunk_start: 0,
            before_hunk_len: 0,
            after_hunk_len: 0,
            old_line_offset,
            new_line_offset,
            context_lines,
            buffer: String::with_capacity(8),
            dst: String::new(),
            interner: &input.interner,
            before: &input.before,
            after: &input.after,
            pos: 0,
        }
    }

    fn print_tokens(&mut self, tokens: &[Token], prefix: char) {
        for &token in tokens {
            writeln!(&mut self.buffer, "{prefix}{}", self.interner[token]).unwrap();
        }
    }

    fn flush(&mut self) {
        if self.before_hunk_len == 0 && self.after_hunk_len == 0 {
            return;
        }

        let end = (self.pos + self.context_lines).min(self.before.len() as u32);
        self.update_pos(end, end);

        writeln!(
            &mut self.dst,
            "@@ -{},{} +{},{} @@",
            self.before_hunk_start + 1 + self.old_line_offset,
            self.before_hunk_len,
            self.after_hunk_start + 1 + self.new_line_offset,
            self.after_hunk_len,
        )
        .unwrap();
        write!(&mut self.dst, "{}", self.buffer).unwrap();
        self.buffer.clear();
        self.before_hunk_len = 0;
        self.after_hunk_len = 0;
    }

    fn update_pos(&mut self, print_to: u32, move_to: u32) {
        self.print_tokens(&self.before[self.pos as usize..print_to as usize], ' ');
        let len = print_to - self.pos;
        self.pos = move_to;
        self.before_hunk_len += len;
        self.after_hunk_len += len;
    }
}

impl OffsetUnifiedDiffBuilder<'_> {
    fn process_change(&mut self, before: Range<u32>, after: Range<u32>) {
        if before.start - self.pos > self.context_lines * 2 {
            self.flush();
        }
        if self.before_hunk_len == 0 && self.after_hunk_len == 0 {
            self.pos = before.start.saturating_sub(self.context_lines);
            self.before_hunk_start = self.pos;
            self.after_hunk_start = after.start.saturating_sub(self.context_lines);
        }
        self.update_pos(before.start, before.end);
        self.before_hunk_len += before.end - before.start;
        self.after_hunk_len += after.end - after.start;
        self.print_tokens(
            &self.before[before.start as usize..before.end as usize],
            '-',
        );
        self.print_tokens(&self.after[after.start as usize..after.end as usize], '+');
    }

    fn finish(mut self) -> String {
        self.flush();
        self.dst
    }
}

/// Computes a diff between two strings, returning a vector of old and new row
/// ranges.
pub fn line_diff(old_text: &str, new_text: &str) -> Vec<(Range<u32>, Range<u32>)> {
    let mut edits = Vec::new();
    let input = InternedInput::new(lines(old_text), lines(new_text));
    diff_internal(&input, &mut |_, _, old_rows, new_rows| {
        edits.push((old_rows, new_rows));
    });
    edits
}

/// Computes a diff between two strings, returning a vector of edits.
///
/// The edits are represented as tuples of byte ranges and replacement strings.
///
/// Internally, this function first performs a line-based diff, and then performs a second
/// word-based diff within hunks that replace small numbers of lines.
pub fn text_diff(old_text: &str, new_text: &str) -> Vec<(Range<usize>, Arc<str>)> {
    text_diff_with_options(old_text, new_text, DiffOptions::default())
}

/// Computes word-level diff ranges between two strings.
///
/// Returns a tuple of (old_ranges, new_ranges) where each vector contains
/// the byte ranges of changed words in the respective text.
pub fn word_diff_ranges(
    old_text: &str,
    new_text: &str,
    options: DiffOptions,
) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    let mut input: InternedInput<&str> = InternedInput::default();
    input.update_before(tokenize(old_text, options.language_scope.clone()));
    input.update_after(tokenize(new_text, options.language_scope));

    let mut old_ranges: Vec<Range<usize>> = Vec::new();
    let mut new_ranges: Vec<Range<usize>> = Vec::new();

    diff_internal(&input, &mut |old_byte_range, new_byte_range, _, _| {
        if !old_byte_range.is_empty() {
            if let Some(last) = old_ranges.last_mut()
                && last.end >= old_byte_range.start
            {
                last.end = old_byte_range.end;
            } else {
                old_ranges.push(old_byte_range);
            }
        }

        if !new_byte_range.is_empty() {
            if let Some(last) = new_ranges.last_mut()
                && last.end >= new_byte_range.start
            {
                last.end = new_byte_range.end;
            } else {
                new_ranges.push(new_byte_range);
            }
        }
    });

    (old_ranges, new_ranges)
}

/// Computes character-level diff between two strings.
///
/// Usually, you should use `text_diff`, which performs a word-wise diff.
pub fn char_diff<'a>(old_text: &'a str, new_text: &'a str) -> Vec<(Range<usize>, &'a str)> {
    let mut input: InternedInput<&str> = InternedInput::default();
    input.update_before(tokenize_chars(old_text));
    input.update_after(tokenize_chars(new_text));
    let mut edits: Vec<(Range<usize>, &str)> = Vec::new();
    diff_internal(&input, &mut |old_byte_range, new_byte_range, _, _| {
        let replacement = if new_byte_range.is_empty() {
            ""
        } else {
            &new_text[new_byte_range]
        };
        edits.push((old_byte_range, replacement));
    });
    edits
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WordDiffKind {
    Modified,
    Inserted,
    Deleted,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BlockWordDiff {
    pub old: Vec<(Range<usize>, WordDiffKind)>,
    pub new: Vec<(Range<usize>, WordDiffKind)>,
}

/// Computes a word diff across a whole block of lines, classifying each changed
/// region as modified, inserted or deleted.
///
/// Lines whose words are the same are matched first, regardless of whitespace,
/// so that re-indented code (e.g. code wrapped in a new call) pairs with its
/// original lines, and words are then only compared within the remaining
/// unmatched lines. Whitespace changes, such as changed indentation, trailing
/// whitespace, line breaks and blank lines, are reported as changed regions of
/// their own.
///
/// Returns `None` when either side is too large or contains no non-whitespace
/// tokens, in which case callers should fall back to a line-based display.
pub fn block_word_diff(
    old_text: &str,
    new_text: &str,
    language_scope: Option<LanguageScope>,
) -> Option<BlockWordDiff> {
    if old_text.len() > MAX_BLOCK_WORD_DIFF_LEN || new_text.len() > MAX_BLOCK_WORD_DIFF_LEN {
        return None;
    }
    let classifier =
        CharClassifier::new(language_scope).scope_context(Some(CharScopeContext::Completion));
    let old = BlockLines::new(old_text, &classifier);
    let new = BlockLines::new(new_text, &classifier);
    if old.tokens.is_empty() || new.tokens.is_empty() {
        return None;
    }

    let mut differ = BlockDiffer {
        old: &old,
        new: &new,
        result: BlockWordDiff::default(),
    };
    let mut matched_tokens = Vec::new();
    let mut line_input: InternedInput<&str> = InternedInput::default();
    line_input.update_before(old.keys.iter().map(String::as_str));
    line_input.update_after(new.keys.iter().map(String::as_str));
    let line_diff = Diff::compute(Algorithm::Histogram, &line_input);
    let mut token_input: InternedInput<&str> = InternedInput::default();
    let mut old_unchanged_line = 0;
    let mut new_unchanged_line = 0;
    for hunk in line_diff.hunks() {
        differ.compare_matched_lines(
            old_unchanged_line..hunk.before.start as usize,
            new_unchanged_line,
            &mut matched_tokens,
        );
        old_unchanged_line = hunk.before.end as usize;
        new_unchanged_line = hunk.after.end as usize;
        differ.highlight_unmatched_blank_lines(hunk.before.clone(), hunk.after.clone());

        let old_tokens = old.token_range_for_lines(hunk.before);
        let new_tokens = new.token_range_for_lines(hunk.after);
        if old_tokens.is_empty() || new_tokens.is_empty() {
            differ.highlight_changed_tokens(old_tokens, new_tokens);
            continue;
        }

        token_input.clear();
        token_input.update_before(old.token_texts(old_tokens.clone()));
        token_input.update_after(new.token_texts(new_tokens.clone()));
        let token_diff = Diff::compute(Algorithm::Histogram, &token_input);
        let mut old_unchanged_start = old_tokens.start;
        let mut new_unchanged_start = new_tokens.start;
        for token_hunk in token_diff.hunks() {
            let old_range = old_tokens.start + token_hunk.before.start as usize
                ..old_tokens.start + token_hunk.before.end as usize;
            let new_range = new_tokens.start + token_hunk.after.start as usize
                ..new_tokens.start + token_hunk.after.end as usize;
            push_matched_tokens(
                old_unchanged_start..old_range.start,
                new_unchanged_start,
                &mut matched_tokens,
            );
            old_unchanged_start = old_range.end;
            new_unchanged_start = new_range.end;
            differ.highlight_changed_tokens(old_range, new_range);
        }
        push_matched_tokens(
            old_unchanged_start..old_tokens.end,
            new_unchanged_start,
            &mut matched_tokens,
        );
    }
    differ.compare_matched_lines(
        old_unchanged_line..old.lines.len(),
        new_unchanged_line,
        &mut matched_tokens,
    );
    matched_tokens.sort_unstable();

    if matched_tokens.first() == Some(&(0, 0)) {
        differ.compare_gaps(0, 0);
    }
    for pair in matched_tokens.windows(2) {
        let [(old_ix, new_ix), (next_old_ix, next_new_ix)] = *pair else {
            continue;
        };
        if next_old_ix == old_ix + 1 && next_new_ix == new_ix + 1 {
            differ.compare_gaps(next_old_ix, next_new_ix);
        }
    }
    if matched_tokens.last() == Some(&(old.tokens.len() - 1, new.tokens.len() - 1)) {
        differ.compare_gaps(old.tokens.len(), new.tokens.len());
    }

    let mut result = differ.result;
    result.old.sort_by_key(|(range, _)| range.start);
    result.new.sort_by_key(|(range, _)| range.start);
    Some(result)
}

/// Pairs up the old tokens in the given range with the new tokens starting at
/// `new_start`, which are known to be equal.
fn push_matched_tokens(
    old_tokens: Range<usize>,
    new_start: usize,
    matched_tokens: &mut Vec<(usize, usize)>,
) {
    let old_start = old_tokens.start;
    matched_tokens.extend(old_tokens.map(|old_ix| (old_ix, new_start + (old_ix - old_start))));
}

#[derive(Clone, Copy)]
enum Side {
    Old,
    New,
}

struct BlockDiffer<'a> {
    old: &'a BlockLines<'a>,
    new: &'a BlockLines<'a>,
    result: BlockWordDiff,
}

impl BlockDiffer<'_> {
    fn push(&mut self, side: Side, range: Range<usize>, kind: WordDiffKind) {
        match side {
            Side::Old => self.result.old.push((range, kind)),
            Side::New => self.result.new.push((range, kind)),
        }
    }

    /// Records the tokens of lines that the line diff matched, and compares
    /// matched blank lines, whose whitespace isn't covered by any token gap.
    fn compare_matched_lines(
        &mut self,
        old_lines: Range<usize>,
        new_start: usize,
        matched_tokens: &mut Vec<(usize, usize)>,
    ) {
        let old_start = old_lines.start;
        for old_line in old_lines {
            let new_line = new_start + (old_line - old_start);
            let old_tokens = self
                .old
                .token_range_for_lines(old_line as u32..old_line as u32 + 1);
            let new_tokens = self
                .new
                .token_range_for_lines(new_line as u32..new_line as u32 + 1);
            if old_tokens.is_empty() {
                self.push_whitespace_diff(
                    self.old.lines[old_line].clone(),
                    self.new.lines[new_line].clone(),
                    Alignment::Start,
                );
            } else {
                push_matched_tokens(old_tokens, new_tokens.start, matched_tokens);
            }
        }
    }

    fn highlight_unmatched_blank_lines(&mut self, old_lines: Range<u32>, new_lines: Range<u32>) {
        for line_ix in old_lines {
            if let Some(range) = self.old.blank_line_range(line_ix as usize) {
                self.push(Side::Old, range, WordDiffKind::Deleted);
            }
        }
        for line_ix in new_lines {
            if let Some(range) = self.new.blank_line_range(line_ix as usize) {
                self.push(Side::New, range, WordDiffKind::Inserted);
            }
        }
    }

    /// Highlights a region of changed tokens along with the whitespace that
    /// changed around it.
    fn highlight_changed_tokens(&mut self, old_tokens: Range<usize>, new_tokens: Range<usize>) {
        let kind = match (old_tokens.is_empty(), new_tokens.is_empty()) {
            (true, true) => return,
            (true, false) => WordDiffKind::Inserted,
            (false, true) => WordDiffKind::Deleted,
            (false, false) => WordDiffKind::Modified,
        };
        for range in self.old.token_region_ranges(old_tokens.clone()) {
            self.push(Side::Old, range, kind);
        }
        for range in self.new.token_region_ranges(new_tokens.clone()) {
            self.push(Side::New, range, kind);
        }

        match kind {
            WordDiffKind::Modified => {
                self.compare_gaps(old_tokens.start, new_tokens.start);
                self.compare_gaps(old_tokens.end, new_tokens.end);
            }
            WordDiffKind::Inserted => self.highlight_gaps_around_region(
                Side::New,
                old_tokens.start,
                new_tokens,
                WordDiffKind::Inserted,
            ),
            WordDiffKind::Deleted => self.highlight_gaps_around_region(
                Side::Old,
                new_tokens.start,
                old_tokens,
                WordDiffKind::Deleted,
            ),
        }
    }

    /// For tokens that only exist on one side, one of the gaps around them
    /// corresponds to the single gap on the other side, and the other gap was
    /// added along with the tokens.
    fn highlight_gaps_around_region(
        &mut self,
        side: Side,
        other_gap_ix: usize,
        tokens: Range<usize>,
        kind: WordDiffKind,
    ) {
        let (lines, other_lines) = match side {
            Side::Old => (self.old, self.new),
            Side::New => (self.new, self.old),
        };
        let other_gap = other_lines.gap(other_gap_ix);
        let before = lines.gap(tokens.start);
        let after = lines.gap(tokens.end);
        let other_gap_text = &other_lines.text[other_gap];
        let added_gap = if lines.text[before.clone()] == *other_gap_text {
            after
        } else if lines.text[after.clone()] == *other_gap_text {
            before
        } else {
            match side {
                Side::Old => self.compare_gaps(tokens.start, other_gap_ix),
                Side::New => self.compare_gaps(other_gap_ix, tokens.start),
            }
            after
        };
        for range in lines.gap_highlight_ranges(added_gap) {
            self.push(side, range, kind);
        }
    }

    /// Compares the whitespace before the given old and new tokens, which is
    /// the whitespace after the last token for indices past the last token.
    fn compare_gaps(&mut self, old_token_ix: usize, new_token_ix: usize) {
        let old_gap = self.old.gap(old_token_ix);
        let new_gap = self.new.gap(new_token_ix);
        if self.old.text[old_gap.clone()] == self.new.text[new_gap.clone()] {
            return;
        }
        let old_breaks = self.old.gap_line_break(old_token_ix);
        let new_breaks = self.new.gap_line_break(new_token_ix);
        match (old_breaks, new_breaks) {
            (Some(old_break), Some(new_break)) => {
                self.push_whitespace_diff(
                    old_gap.start..old_break.start,
                    new_gap.start..new_break.start,
                    Alignment::Start,
                );
                self.push_whitespace_diff(
                    old_break.end..old_gap.end,
                    new_break.end..new_gap.end,
                    Alignment::End,
                );
            }
            (None, None) => self.push_whitespace_diff(old_gap, new_gap, Alignment::Start),
            _ => {
                let kind = if old_gap.is_empty() {
                    WordDiffKind::Inserted
                } else if new_gap.is_empty() {
                    WordDiffKind::Deleted
                } else {
                    WordDiffKind::Modified
                };
                for range in self.old.gap_highlight_ranges(old_gap) {
                    self.push(Side::Old, range, kind);
                }
                for range in self.new.gap_highlight_ranges(new_gap) {
                    self.push(Side::New, range, kind);
                }
            }
        }
    }

    /// Pushes the difference between two runs of whitespace, ignoring their
    /// common prefix or suffix depending on the alignment.
    fn push_whitespace_diff(
        &mut self,
        old_range: Range<usize>,
        new_range: Range<usize>,
        alignment: Alignment,
    ) {
        let old_whitespace = &self.old.text[old_range.clone()];
        let new_whitespace = &self.new.text[new_range.clone()];
        let common_len = match alignment {
            Alignment::Start => old_whitespace
                .char_indices()
                .zip(new_whitespace.chars())
                .find(|((_, old_char), new_char)| old_char != new_char)
                .map_or(
                    old_whitespace.len().min(new_whitespace.len()),
                    |((ix, _), _)| ix,
                ),
            Alignment::End => old_whitespace
                .chars()
                .rev()
                .zip(new_whitespace.chars().rev())
                .take_while(|(old_char, new_char)| old_char == new_char)
                .map(|(old_char, _)| old_char.len_utf8())
                .sum(),
        };
        let (old_changed, new_changed) = match alignment {
            Alignment::Start => (
                old_range.start + common_len..old_range.end,
                new_range.start + common_len..new_range.end,
            ),
            Alignment::End => (
                old_range.start..old_range.end - common_len,
                new_range.start..new_range.end - common_len,
            ),
        };
        match (old_changed.is_empty(), new_changed.is_empty()) {
            (true, true) => {}
            (true, false) => self.push(Side::New, new_changed, WordDiffKind::Inserted),
            (false, true) => self.push(Side::Old, old_changed, WordDiffKind::Deleted),
            (false, false) => {
                self.push(Side::Old, old_changed, WordDiffKind::Modified);
                self.push(Side::New, new_changed, WordDiffKind::Modified);
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Alignment {
    /// Compare from the start, e.g. for whitespace after a token.
    Start,
    /// Compare from the end, e.g. for indentation, so that added indentation
    /// is reported at the start of the line.
    End,
}

/// Delimiters are always separate tokens, because wrapping code in a new call
/// adds closing delimiters next to existing ones (e.g. `)` becomes `),`), while
/// other punctuation runs such as `==` or `=>` stay single tokens.
fn is_delimiter(c: char) -> bool {
    matches!(c, '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';')
}

/// The non-whitespace tokens of a block of text, grouped by line.
struct BlockLines<'a> {
    text: &'a str,
    tokens: Vec<Range<usize>>,
    token_lines: Vec<usize>,
    /// For each line, the index of its first token; has one extra trailing entry.
    line_token_starts: Vec<usize>,
    /// For each line, the range of its text without the line break.
    lines: Vec<Range<usize>>,
    /// For each line, its tokens joined by single spaces, used to match lines
    /// regardless of whitespace.
    keys: Vec<String>,
}

impl<'a> BlockLines<'a> {
    fn new(text: &'a str, classifier: &CharClassifier) -> Self {
        let mut tokens = Vec::new();
        let mut token_lines = Vec::new();
        let mut line_token_starts = Vec::new();
        let mut lines = Vec::new();
        let mut keys = Vec::new();
        let mut line_start = 0;
        for line in text.split_inclusive('\n') {
            line_token_starts.push(tokens.len());
            lines.push(line_start..line_start + line.trim_end_matches('\n').len());
            let mut key = String::new();
            let mut previous_was_whitespace = false;
            let mut chars = line.char_indices().peekable();
            while let Some((ix, c)) = chars.next() {
                let start = line_start + ix;
                match classifier.kind(c) {
                    CharKind::Whitespace => {
                        previous_was_whitespace = true;
                        continue;
                    }
                    CharKind::Punctuation if is_delimiter(c) => {
                        tokens.push(start..start + c.len_utf8())
                    }
                    kind => {
                        let mut end = start + c.len_utf8();
                        while let Some((next_ix, next_c)) = chars.peek().copied()
                            && classifier.kind(next_c) == kind
                            && !is_delimiter(next_c)
                        {
                            end = line_start + next_ix + next_c.len_utf8();
                            chars.next();
                        }
                        tokens.push(start..end);
                    }
                }
                token_lines.push(lines.len() - 1);
                if previous_was_whitespace && !key.is_empty() {
                    key.push(' ');
                }
                previous_was_whitespace = false;
                if let Some(token) = tokens.last() {
                    key.push_str(&text[token.clone()]);
                }
            }
            keys.push(key);
            line_start += line.len();
        }
        line_token_starts.push(tokens.len());
        Self {
            text,
            tokens,
            token_lines,
            line_token_starts,
            lines,
            keys,
        }
    }

    fn token_range_for_lines(&self, lines: Range<u32>) -> Range<usize> {
        self.line_token_starts[lines.start as usize]..self.line_token_starts[lines.end as usize]
    }

    fn token_texts(&self, tokens: Range<usize>) -> impl Iterator<Item = &'a str> + '_ {
        self.tokens[tokens]
            .iter()
            .map(|token| &self.text[token.clone()])
    }

    /// The whitespace before the token at the given index, or after the last
    /// token for an index past the last token.
    fn gap(&self, token_ix: usize) -> Range<usize> {
        let start = match token_ix.checked_sub(1) {
            Some(previous_ix) => self.tokens[previous_ix].end,
            None => 0,
        };
        let end = self
            .tokens
            .get(token_ix)
            .map_or(self.text.len(), |token| token.start);
        start..end
    }

    /// The part of the gap before the given token that spans line breaks, from
    /// the first line break up to the start of the last line. The block start
    /// counts as a line break, so that the first line's indentation is
    /// compared as indentation.
    fn gap_line_break(&self, token_ix: usize) -> Option<Range<usize>> {
        let gap = self.gap(token_ix);
        let gap_text = &self.text[gap.clone()];
        let last_line_start = gap_text.rfind('\n').map(|ix| gap.start + ix + 1);
        if token_ix == 0 {
            return Some(gap.start..last_line_start.unwrap_or(gap.start));
        }
        let first_break = gap.start + gap_text.find('\n')?;
        Some(first_break..last_line_start.unwrap_or(first_break))
    }

    /// The ranges to highlight for a whole gap that was added or removed: the
    /// whitespace at the end of its first line and at the start of its last
    /// line, or the line break itself if both are empty.
    fn gap_highlight_ranges(&self, gap: Range<usize>) -> Vec<Range<usize>> {
        let gap_text = &self.text[gap.clone()];
        let (Some(first_break), Some(last_break)) = (gap_text.find('\n'), gap_text.rfind('\n'))
        else {
            return if gap.is_empty() {
                Vec::new()
            } else {
                vec![gap]
            };
        };
        let ranges = [
            gap.start..gap.start + first_break,
            gap.start + last_break + 1..gap.end,
        ]
        .into_iter()
        .filter(|range| !range.is_empty())
        .collect::<Vec<_>>();
        if ranges.is_empty() {
            let line_break = gap.start + first_break;
            vec![line_break..line_break + 1]
        } else {
            ranges
        }
    }

    /// The range to highlight for a line without tokens: its whitespace, or its
    /// line break if it is empty.
    fn blank_line_range(&self, line_ix: usize) -> Option<Range<usize>> {
        if self.line_token_starts[line_ix] != self.line_token_starts[line_ix + 1] {
            return None;
        }
        let line = self.lines[line_ix].clone();
        if !line.is_empty() {
            Some(line)
        } else if self.text[line.end..].starts_with('\n') {
            Some(line.end..line.end + 1)
        } else {
            None
        }
    }

    /// The ranges covered by the given tokens, one per line. Lines that the
    /// region continues from or onto are extended to the line start or end, so
    /// that the whitespace within the region is highlighted too.
    fn token_region_ranges(&self, tokens: Range<usize>) -> Vec<Range<usize>> {
        let Some(last_line) = tokens.clone().last().map(|ix| self.token_lines[ix]) else {
            return Vec::new();
        };
        let first_line = self.token_lines[tokens.start];
        let mut ranges: Vec<Range<usize>> = Vec::new();
        let mut current_line = None;
        for token_ix in tokens {
            let line_ix = self.token_lines[token_ix];
            let token = self.tokens[token_ix].clone();
            match (current_line, ranges.last_mut()) {
                (Some(current_line), Some(range)) if current_line == line_ix => {
                    range.end = token.end;
                }
                _ => {
                    ranges.push(token);
                    current_line = Some(line_ix);
                }
            }
        }
        for (line_ix, range) in (first_line..=last_line)
            .filter(|line_ix| {
                self.line_token_starts[*line_ix] != self.line_token_starts[line_ix + 1]
            })
            .zip(ranges.iter_mut())
        {
            if line_ix != first_line {
                range.start = self.lines[line_ix].start;
            }
            if line_ix != last_line {
                range.end = self.lines[line_ix].end;
            }
        }
        ranges
    }
}

pub struct DiffOptions {
    pub language_scope: Option<LanguageScope>,
    pub max_word_diff_len: usize,
    pub max_word_diff_line_count: usize,
    pub word_diff_style: WordDiffStyle,
}

impl Default for DiffOptions {
    fn default() -> Self {
        Self {
            language_scope: Default::default(),
            max_word_diff_len: MAX_WORD_DIFF_LEN,
            max_word_diff_line_count: MAX_WORD_DIFF_LINE_COUNT,
            word_diff_style: WordDiffStyle::Lines,
        }
    }
}

/// Computes a diff between two strings, using a specific language scope's
/// word characters for word-level diffing.
pub fn text_diff_with_options(
    old_text: &str,
    new_text: &str,
    options: DiffOptions,
) -> Vec<(Range<usize>, Arc<str>)> {
    let empty: Arc<str> = Arc::default();
    let mut edits = Vec::new();
    let mut hunk_input = InternedInput::default();
    let input = InternedInput::new(lines(old_text), lines(new_text));
    diff_internal(&input, &mut |old_byte_range,
                                new_byte_range,
                                old_rows,
                                new_rows| {
        if should_perform_word_diff_within_hunk(
            &old_rows,
            &old_byte_range,
            &new_rows,
            &new_byte_range,
            &options,
        ) {
            let old_offset = old_byte_range.start;
            let new_offset = new_byte_range.start;
            hunk_input.clear();
            hunk_input.update_before(tokenize(
                &old_text[old_byte_range],
                options.language_scope.clone(),
            ));
            hunk_input.update_after(tokenize(
                &new_text[new_byte_range],
                options.language_scope.clone(),
            ));
            diff_internal(&hunk_input, &mut |old_byte_range, new_byte_range, _, _| {
                let old_byte_range =
                    old_offset + old_byte_range.start..old_offset + old_byte_range.end;
                let new_byte_range =
                    new_offset + new_byte_range.start..new_offset + new_byte_range.end;
                let replacement_text = if new_byte_range.is_empty() {
                    empty.clone()
                } else {
                    new_text[new_byte_range].into()
                };
                edits.push((old_byte_range, replacement_text));
            });
        } else {
            let replacement_text = if new_byte_range.is_empty() {
                empty.clone()
            } else {
                new_text[new_byte_range].into()
            };
            edits.push((old_byte_range, replacement_text));
        }
    });
    edits
}

pub fn apply_diff_patch(base_text: &str, patch: &str) -> Result<String, anyhow::Error> {
    let patch = diffy::Patch::from_str(patch).context("Failed to parse patch")?;
    let result = diffy::apply(base_text, &patch);
    result.map_err(|err| anyhow!(err))
}

pub fn apply_reversed_diff_patch(base_text: &str, patch: &str) -> Result<String, anyhow::Error> {
    let patch = diffy::Patch::from_str(patch).context("Failed to parse patch")?;
    let reversed = patch.reverse();
    diffy::apply(base_text, &reversed).map_err(|err| anyhow!(err))
}

fn should_perform_word_diff_within_hunk(
    old_row_range: &Range<u32>,
    old_byte_range: &Range<usize>,
    new_row_range: &Range<u32>,
    new_byte_range: &Range<usize>,
    options: &DiffOptions,
) -> bool {
    !old_byte_range.is_empty()
        && !new_byte_range.is_empty()
        && old_byte_range.len() <= options.max_word_diff_len
        && new_byte_range.len() <= options.max_word_diff_len
        && old_row_range.len() <= options.max_word_diff_line_count
        && new_row_range.len() <= options.max_word_diff_line_count
}

fn diff_internal(
    input: &InternedInput<&str>,
    on_change: &mut dyn FnMut(Range<usize>, Range<usize>, Range<u32>, Range<u32>),
) {
    let mut old_offset = 0;
    let mut new_offset = 0;
    let mut old_token_ix = 0;
    let mut new_token_ix = 0;
    let diff = Diff::compute(Algorithm::Histogram, input);
    for hunk in diff.hunks() {
        let old_tokens = hunk.before;
        let new_tokens = hunk.after;
        old_offset += token_len(
            input,
            &input.before[old_token_ix as usize..old_tokens.start as usize],
        );
        new_offset += token_len(
            input,
            &input.after[new_token_ix as usize..new_tokens.start as usize],
        );
        let old_len = token_len(
            input,
            &input.before[old_tokens.start as usize..old_tokens.end as usize],
        );
        let new_len = token_len(
            input,
            &input.after[new_tokens.start as usize..new_tokens.end as usize],
        );
        let old_byte_range = old_offset..old_offset + old_len;
        let new_byte_range = new_offset..new_offset + new_len;
        old_token_ix = old_tokens.end;
        new_token_ix = new_tokens.end;
        old_offset = old_byte_range.end;
        new_offset = new_byte_range.end;
        on_change(old_byte_range, new_byte_range, old_tokens, new_tokens);
    }
}

fn tokenize_chars(text: &str) -> impl Iterator<Item = &str> {
    let mut chars = text.char_indices().peekable();
    iter::from_fn(move || {
        let (start, c) = chars.next()?;
        Some(&text[start..start + c.len_utf8()])
    })
}

fn tokenize(text: &str, language_scope: Option<LanguageScope>) -> impl Iterator<Item = &str> {
    let classifier =
        CharClassifier::new(language_scope).scope_context(Some(CharScopeContext::Completion));
    let mut chars = text.char_indices();
    let mut prev = None;
    let mut start_ix = 0;
    iter::from_fn(move || {
        for (ix, c) in chars.by_ref() {
            let mut token = None;
            let kind = classifier.kind(c);
            if let Some((prev_char, prev_kind)) = prev
                && (kind != prev_kind || (kind == CharKind::Punctuation && c != prev_char))
            {
                token = Some(&text[start_ix..ix]);
                start_ix = ix;
            }
            prev = Some((c, kind));
            if token.is_some() {
                return token;
            }
        }
        if start_ix < text.len() {
            let token = &text[start_ix..];
            start_ix = text.len();
            return Some(token);
        }
        None
    })
}

fn token_len(input: &InternedInput<&str>, tokens: &[Token]) -> usize {
    tokens
        .iter()
        .map(|token| input.interner[*token].len())
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tokenize() {
        let text = "";
        assert_eq!(tokenize(text, None).collect::<Vec<_>>(), Vec::<&str>::new());

        let text = " ";
        assert_eq!(tokenize(text, None).collect::<Vec<_>>(), vec![" "]);

        let text = "one";
        assert_eq!(tokenize(text, None).collect::<Vec<_>>(), vec!["one"]);

        let text = "one\n";
        assert_eq!(tokenize(text, None).collect::<Vec<_>>(), vec!["one", "\n"]);

        let text = "one.two(three)";
        assert_eq!(
            tokenize(text, None).collect::<Vec<_>>(),
            vec!["one", ".", "two", "(", "three", ")"]
        );

        let text = "one two three()";
        assert_eq!(
            tokenize(text, None).collect::<Vec<_>>(),
            vec!["one", " ", "two", " ", "three", "(", ")"]
        );

        let text = "   one\n two three";
        assert_eq!(
            tokenize(text, None).collect::<Vec<_>>(),
            vec!["   ", "one", "\n ", "two", " ", "three"]
        );
    }

    #[test]
    fn test_text_diff() {
        let old_text = "one two three";
        let new_text = "one TWO three";
        assert_eq!(text_diff(old_text, new_text), [(4..7, "TWO".into()),]);

        let old_text = "one\ntwo\nthree\n";
        let new_text = "one\ntwo\nAND\nTHEN\nthree\n";
        assert_eq!(
            text_diff(old_text, new_text),
            [(8..8, "AND\nTHEN\n".into()),]
        );

        let old_text = "one two\nthree four five\nsix seven eight nine\nten\n";
        let new_text = "one two\nthree FOUR five\nsix SEVEN eight nine\nten\nELEVEN\n";
        assert_eq!(
            text_diff(old_text, new_text),
            [
                (14..18, "FOUR".into()),
                (28..33, "SEVEN".into()),
                (49..49, "ELEVEN\n".into())
            ]
        );
    }

    fn render_block_word_diff<'a>(
        old_text: &'a str,
        new_text: &'a str,
    ) -> Option<(Vec<(&'a str, WordDiffKind)>, Vec<(&'a str, WordDiffKind)>)> {
        let diff = block_word_diff(old_text, new_text, None)?;
        let render = |text: &'a str, ranges: Vec<(Range<usize>, WordDiffKind)>| {
            ranges
                .into_iter()
                .map(|(range, kind)| (&text[range], kind))
                .collect::<Vec<_>>()
        };
        Some((render(old_text, diff.old), render(new_text, diff.new)))
    }

    /// Renders ranges as `row:column text` to also check where they are.
    fn render_positions(text: &str, ranges: &[(Range<usize>, WordDiffKind)]) -> Vec<String> {
        ranges
            .iter()
            .map(|(range, _)| {
                let row = text[..range.start].matches('\n').count();
                let column = range.start - text[..range.start].rfind('\n').map_or(0, |ix| ix + 1);
                format!("{row}:{column} {:?}", &text[range.clone()])
            })
            .collect()
    }

    #[test]
    fn test_block_word_diff_widget_wrap() {
        let old_text = indoc::indoc! {"
            return Foo(
              a: 1,
            );
        "};
        let new_text = indoc::indoc! {"
            return Padding(
              padding: 8,
              child: Foo(
                a: 1,
              ),
            );
        "};
        let diff = block_word_diff(old_text, new_text, None).unwrap();
        assert_eq!(diff.old, vec![]);
        assert_eq!(
            render_positions(new_text, &diff.new),
            vec![
                "0:7 \"Padding(\"",
                "1:0 \"  padding: 8,\"",
                "2:0 \"  child:\"",
                "2:8 \" \"",
                "3:0 \"  \"",
                "4:0 \"  \"",
                "4:2 \"),\"",
            ]
        );
    }

    #[test]
    fn test_block_word_diff_widget_wrap_with_edits() {
        let old_text = indoc::indoc! {"
            return ValueListenableBuilder<HomeItem>(
              valueListenable: vm.selected,
              builder: (context, selectedItem, _) {
                final model = itemModels.firstWhere(
                  (item) => item.id == selectedItem,
                  orElse: () => itemModels.first,
                );
                return model.pageBuilder?.call(context, navigationBar) ?? Container();
              },
        "};
        let new_text = indoc::indoc! {"
            return Padding(
              padding: const EdgeInsets.all(8.0),
              child: ValueListenableBuilder<HomeItem>(
                valueListenable: vm.selected,
                builder: (context, selectedItem, _) {
                  final model = itemModels.firstWhereOrNull(
                    (item) => item.id != null,
                  );
                  return model.somethingElse?.call(context, navigationBar) ?? Container();
                },
              ),
        "};
        let diff = block_word_diff(old_text, new_text, None).unwrap();
        assert_eq!(
            render_positions(old_text, &diff.old),
            vec![
                "3:29 \"firstWhere\"",
                "4:24 \"== selectedItem,\"",
                "5:0 \"      orElse: () => itemModels.first\"",
                "7:17 \"pageBuilder\"",
            ]
        );
        assert_eq!(
            render_positions(new_text, &diff.new),
            vec![
                "0:7 \"Padding(\"",
                "1:0 \"  padding: const EdgeInsets.all(8.0),\"",
                "2:0 \"  child:\"",
                "2:8 \" \"",
                "3:0 \"  \"",
                "4:0 \"  \"",
                "5:0 \"  \"",
                "5:31 \"firstWhereOrNull\"",
                "6:0 \"  \"",
                "6:26 \"!= null\"",
                "7:0 \"  \"",
                "8:0 \"  \"",
                "8:19 \"somethingElse\"",
                "9:0 \"  \"",
                "10:0 \"  \"",
                "10:2 \"),\"",
            ]
        );
    }

    #[test]
    fn test_block_word_diff_nested_call() {
        let diff = block_word_diff("Foo()\n", "Padding(child: Foo())\n", None).unwrap();
        assert_eq!(diff.old, vec![]);
        assert!(!diff.new.is_empty());
    }

    #[test]
    fn test_block_word_diff_highlights_indentation_at_line_start() {
        let old_text = "foo(\n  a,\n)\n";
        let new_text = "  foo(\n    a,\n  )\n";
        let diff = block_word_diff(old_text, new_text, None).unwrap();
        assert_eq!(
            render_positions(new_text, &diff.new),
            vec!["0:0 \"  \"", "1:0 \"  \"", "2:0 \"  \""]
        );
        assert!(
            diff.new
                .iter()
                .all(|(_, kind)| *kind == WordDiffKind::Inserted)
        );

        let diff = block_word_diff(new_text, old_text, None).unwrap();
        assert_eq!(
            render_positions(new_text, &diff.old),
            vec!["0:0 \"  \"", "1:0 \"  \"", "2:0 \"  \""]
        );
        assert!(
            diff.old
                .iter()
                .all(|(_, kind)| *kind == WordDiffKind::Deleted)
        );

        assert_eq!(
            render_block_word_diff("\tfoo\n", "  foo\n"),
            Some((
                vec![("\t", WordDiffKind::Modified)],
                vec![("  ", WordDiffKind::Modified)]
            ))
        );
    }

    #[test]
    fn test_block_word_diff_highlights_whitespace_within_lines() {
        assert_eq!(
            render_block_word_diff("var total=1+2;\n", "var total = 1 + 2;\n"),
            Some((
                vec![],
                vec![
                    (" ", WordDiffKind::Inserted),
                    (" ", WordDiffKind::Inserted),
                    (" ", WordDiffKind::Inserted),
                    (" ", WordDiffKind::Inserted),
                ]
            ))
        );
        assert_eq!(
            render_block_word_diff("x = \"hello world\";\n", "x = \"hello  world\";\n"),
            Some((vec![], vec![(" ", WordDiffKind::Inserted)]))
        );
        assert_eq!(
            render_block_word_diff("a  b\n", "a\tb\n"),
            Some((
                vec![("  ", WordDiffKind::Modified)],
                vec![("\t", WordDiffKind::Modified)]
            ))
        );
    }

    #[test]
    fn test_block_word_diff_highlights_trailing_whitespace() {
        let diff = block_word_diff("a\nb\n", "a  \nb\n", None).unwrap();
        assert_eq!(render_positions("a  \nb\n", &diff.new), vec!["0:1 \"  \""]);
        assert_eq!(
            render_block_word_diff("a b  \n", "a b\n"),
            Some((vec![("  ", WordDiffKind::Deleted)], vec![]))
        );
    }

    #[test]
    fn test_block_word_diff_highlights_blank_lines() {
        let diff = block_word_diff("a\nb\n", "a\n\nb\n", None).unwrap();
        assert_eq!(render_positions("a\n\nb\n", &diff.new), vec!["1:0 \"\\n\""]);

        let diff = block_word_diff("a\n\nb\n", "a\nb\n", None).unwrap();
        assert_eq!(render_positions("a\n\nb\n", &diff.old), vec!["1:0 \"\\n\""]);

        let diff = block_word_diff("a\n\nb\n", "a\n  \nb\n", None).unwrap();
        assert_eq!(
            render_positions("a\n  \nb\n", &diff.new),
            vec!["1:0 \"  \""]
        );
    }

    #[test]
    fn test_block_word_diff_highlights_line_breaks() {
        let diff = block_word_diff("foo(1)\n", "foo(\n  1,\n)\n", None).unwrap();
        assert_eq!(
            render_positions("foo(\n  1,\n)\n", &diff.new),
            vec!["1:0 \"  \"", "1:3 \",\"", "1:4 \"\\n\""]
        );

        assert_eq!(
            render_block_word_diff("a b\n", "a\nb\n"),
            Some((
                vec![(" ", WordDiffKind::Modified)],
                vec![("\n", WordDiffKind::Modified)]
            ))
        );
    }

    #[test]
    fn test_block_word_diff_modified_words() {
        assert_eq!(
            render_block_word_diff("let x = foo(1);\n", "let x = bar(1);\n"),
            Some((
                vec![("foo", WordDiffKind::Modified)],
                vec![("bar", WordDiffKind::Modified)]
            ))
        );
        assert_eq!(
            render_block_word_diff("日本 foo\n", "日本 bar\n"),
            Some((
                vec![("foo", WordDiffKind::Modified)],
                vec![("bar", WordDiffKind::Modified)]
            ))
        );
    }

    #[test]
    fn test_block_word_diff_deleted_line() {
        assert_eq!(
            render_block_word_diff("a(\n  b,\n  c,\n)\n", "a(\n  b,\n)\n"),
            Some((
                vec![("  ", WordDiffKind::Deleted), ("c,", WordDiffKind::Deleted)],
                vec![]
            ))
        );
    }

    #[test]
    fn test_block_word_diff_splits_regions_per_line() {
        assert_eq!(
            render_block_word_diff("x\n", "one two\n  three\n"),
            Some((
                vec![("x", WordDiffKind::Modified)],
                vec![
                    ("one two", WordDiffKind::Modified),
                    ("  three", WordDiffKind::Modified)
                ]
            ))
        );
    }

    #[test]
    fn test_block_word_diff_skips_unsuitable_input() {
        let long_text = "a ".repeat(MAX_BLOCK_WORD_DIFF_LEN);
        assert_eq!(block_word_diff(&long_text, "a\n", None), None);
        assert_eq!(block_word_diff("\n  \n", "foo\n", None), None);
        assert_eq!(block_word_diff("foo\n", "\n", None), None);
    }

    #[test]
    fn test_apply_diff_patch() {
        let old_text = "one two\nthree four five\nsix seven eight nine\nten\n";
        let new_text = "one two\nthree FOUR five\nsix SEVEN eight nine\nten\nELEVEN\n";
        let patch = unified_diff(old_text, new_text);
        assert_eq!(apply_diff_patch(old_text, &patch).unwrap(), new_text);
    }

    #[test]
    fn test_apply_reversed_diff_patch() {
        let old_text = "one two\nthree four five\nsix seven eight nine\nten\n";
        let new_text = "one two\nthree FOUR five\nsix SEVEN eight nine\nten\nELEVEN\n";
        let patch = unified_diff(old_text, new_text);
        assert_eq!(
            apply_reversed_diff_patch(new_text, &patch).unwrap(),
            old_text
        );
    }

    #[test]
    fn test_char_diff() {
        assert_eq!(char_diff("", ""), vec![]);
        assert_eq!(char_diff("", "abc"), vec![(0..0, "abc")]);
        assert_eq!(char_diff("abc", ""), vec![(0..3, "")]);
        assert_eq!(char_diff("ac", "abc"), vec![(1..1, "b")]); // "b" inserted
        assert_eq!(char_diff("abc", "ac"), vec![(1..2, "")]); // "b" deleted
        assert_eq!(char_diff("abc", "adc"), vec![(1..2, "d")]); // "b" replaced with "d"
        assert_eq!(char_diff("日", "日本語"), vec![(3..3, "本語")]); // "本語" inserted
        assert_eq!(char_diff("日本語", "日"), vec![(3..9, "")]); // "本語" deleted
        assert_eq!(char_diff("🎉", "🎉🎊🎈"), vec![(4..4, "🎊🎈")]); // "🎊🎈" inserted
        assert_eq!(
            char_diff("test日本", "test日本語です"),
            vec![(10..10, "語です")]
        );
    }

    #[test]
    fn test_unified_diff_with_offsets() {
        let old_text = "foo\nbar\nbaz\n";
        let new_text = "foo\nBAR\nbaz\n";

        let expected_diff_body = " foo\n-bar\n+BAR\n baz\n";

        let diff_no_offset = unified_diff(old_text, new_text);
        assert_eq!(
            diff_no_offset,
            format!("@@ -1,3 +1,3 @@\n{}", expected_diff_body)
        );

        let diff_with_offset = unified_diff_with_offsets(old_text, new_text, 9, 11);
        assert_eq!(
            diff_with_offset,
            format!("@@ -10,3 +12,3 @@\n{}", expected_diff_body)
        );

        let diff_with_offset = unified_diff_with_offsets(old_text, new_text, 99, 104);
        assert_eq!(
            diff_with_offset,
            format!("@@ -100,3 +105,3 @@\n{}", expected_diff_body)
        );
    }

    #[test]
    fn test_unified_diff_with_context() {
        // Test that full context includes all lines from the start
        let old_text = "line1\nline2\nline3\nline4\nline5\nCHANGE_ME\nline7\nline8\n";
        let new_text = "line1\nline2\nline3\nline4\nline5\nCHANGED\nline7\nline8\n";

        // With default 3 lines of context, the diff starts at line 3
        let diff_default = unified_diff_with_offsets(old_text, new_text, 0, 0);
        assert_eq!(
            diff_default,
            "@@ -3,6 +3,6 @@\n line3\n line4\n line5\n-CHANGE_ME\n+CHANGED\n line7\n line8\n"
        );

        // With full context (8 lines), the diff starts at line 1
        let diff_full_context = unified_diff_with_context(old_text, new_text, 0, 0, 8);
        assert_eq!(
            diff_full_context,
            "@@ -1,8 +1,8 @@\n line1\n line2\n line3\n line4\n line5\n-CHANGE_ME\n+CHANGED\n line7\n line8\n"
        );

        // With 0 context, only the changed line is shown
        let diff_no_context = unified_diff_with_context(old_text, new_text, 0, 0, 0);
        assert_eq!(diff_no_context, "@@ -6,1 +6,1 @@\n-CHANGE_ME\n+CHANGED\n");
    }
}

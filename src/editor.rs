use std::cell::RefCell;
use std::cmp;
use std::io::{self, Write};
use termion::{self, clear, color, cursor};

use context::ColorClosure;
use Context;
use Buffer;
use event::*;
use util;

/// Buffer for prompt writes, meant to be shared between prompt creations.
struct LocalBuffer(pub RefCell<Vec<u8>>);

impl LocalBuffer {
    pub fn new() -> Self {
        LocalBuffer(RefCell::new(Vec::with_capacity(512)))
    }

    pub fn append(&self, bytes: &[u8]) {
        self.0.borrow_mut().extend_from_slice(bytes);
    }

    pub fn push(&self, byte: u8) {
        self.0.borrow_mut().push(byte);
    }

    pub fn pop(&self) -> Option<u8> {
        self.0.borrow_mut().pop()
    }

    pub fn extract<T>(&self, mut func: impl FnMut(&[u8]) -> T) -> T {
        let result = func(&self.0.borrow());
        self.0.borrow_mut().clear();
        result
    }
}

thread_local! {
    static BUFFER: LocalBuffer = LocalBuffer::new();
}

/// Represents the position of the cursor relative to words in the buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorPosition {
    /// The cursor is in the word with the specified index.
    InWord(usize),

    /// The cursor is on the left edge of the word with the specified index.
    /// For example: `abc |hi`, where `|` is the cursor.
    OnWordLeftEdge(usize),

    /// The cursor is on the right edge of the word with the specified index.
    /// For example: `abc| hi`, where `|` is the cursor.
    OnWordRightEdge(usize),

    /// The cursor is not in contact with any word. Each `Option<usize>` specifies the index of the
    /// closest word to the left and right, respectively, or `None` if there is no word on that side.
    InSpace(Option<usize>, Option<usize>),
}

impl CursorPosition {
    pub fn get(cursor: usize, words: &[(usize, usize)]) -> CursorPosition {
        use CursorPosition::*;

        if words.is_empty() {
            return InSpace(None, None);
        } else if cursor == words[0].0 {
            return OnWordLeftEdge(0);
        } else if cursor < words[0].0 {
            return InSpace(None, Some(0));
        }

        for (i, &(start, end)) in words.iter().enumerate() {
            if start == cursor {
                return OnWordLeftEdge(i);
            } else if end == cursor {
                return OnWordRightEdge(i);
            } else if start < cursor && cursor < end {
                return InWord(i);
            } else if cursor < start {
                return InSpace(Some(i - 1), Some(i));
            }
        }

        InSpace(Some(words.len() - 1), None)
    }
}

/// The core line editor. Displays and provides editing for history and the new buffer.
pub struct Editor<'a, W: Write> {
    prompt: String,
    out: W,
    context: &'a mut Context,

    // A closure that is evaluated just before we write to out.
    // This allows us to do custom syntax highlighting and other fun stuff.
    closure: Option<ColorClosure>,

    // The location of the cursor. Note that the cursor does not lie on a char, but between chars.
    // So, if `cursor == 0` then the cursor is before the first char,
    // and if `cursor == 1` ten the cursor is after the first char and before the second char.
    cursor: usize,

    // Buffer for the new line (ie. not from editing history)
    new_buf: Buffer,

    // None if we're on the new buffer, else the index of history
    cur_history_loc: Option<usize>,

    // The line of the cursor relative to the prompt. 1-indexed.
    // So if the cursor is on the same line as the prompt, `term_cursor_line == 1`.
    // If the cursor is on the line below the prompt, `term_cursor_line == 2`.
    term_cursor_line: usize,

    // The next completion to suggest, or none
    show_completions_hint: Option<(Vec<String>, Option<usize>)>,

    // Show autosuggestions based on history
    show_autosuggestions: bool,

    // if set, the cursor will not be allow to move one past the end of the line, this is necessary
    // for Vi's normal mode.
    pub no_eol: bool,

    no_newline: bool,

    reverse_search: bool,
    forward_search: bool,
    buffer_changed: bool,

    history_subset_index: Vec<usize>,
    history_subset_loc: Option<usize>,

    autosuggestion: Option<Buffer>,
}

macro_rules! cur_buf_mut {
    ($s:expr) => {
        match $s.cur_history_loc {
            Some(i) => {
                $s.buffer_changed = true;
                &mut $s.context.history[i]
            },
            _ => {
                $s.buffer_changed = true;
                &mut $s.new_buf
            },
        }
    }
}

macro_rules! cur_buf {
    ($s:expr) => {
        match $s.cur_history_loc {
            Some(i) => &$s.context.history[i],
            _ => &$s.new_buf,
        }
    }
}

impl<'a, W: Write> Editor<'a, W> {
    pub fn new<P: Into<String>>(
        out: W,
        prompt: P,
        f: Option<ColorClosure>,
        context: &'a mut Context
    ) -> io::Result<Self> {
        Editor::new_with_init_buffer(out, prompt, f, context, Buffer::new())
    }

    pub fn new_with_init_buffer<P: Into<String>, B: Into<Buffer>>(
        out: W,
        prompt: P,
        f: Option<ColorClosure>,
        context: &'a mut Context,
        buffer: B,
    ) -> io::Result<Self> {
        let mut ed = Editor {
            prompt: prompt.into(),
            cursor: 0,
            out: out,
            closure: f,
            new_buf: buffer.into(),
            cur_history_loc: None,
            context: context,
            show_completions_hint: None,
            show_autosuggestions: true,
            term_cursor_line: 1,
            no_eol: false,
            no_newline: false,
            reverse_search: false,
            forward_search: false,
            buffer_changed: false,
            history_subset_index: vec![],
            history_subset_loc: None,
            autosuggestion: None,
        };

        if !ed.new_buf.is_empty() {
            ed.move_cursor_to_end_of_line()?;
        }
        ed.display()?;
        Ok(ed)
    }

    fn is_search(&self) -> bool {
        self.reverse_search || self.forward_search
    }

    fn clear_search(&mut self) {
        self.reverse_search = false;
        self.forward_search = false;
        self.history_subset_loc = None;
        self.history_subset_index.clear();
    }

    /// None if we're on the new buffer, else the index of history
    pub fn current_history_location(&self) -> Option<usize> {
        self.cur_history_loc
    }

    pub fn get_words_and_cursor_position(&self) -> (Vec<(usize, usize)>, CursorPosition) {
        let word_fn = &self.context.word_divider_fn;
        let words = word_fn(cur_buf!(self));
        let pos = CursorPosition::get(self.cursor, &words);
        (words, pos)
    }

    pub fn set_prompt(&mut self, prompt: String) {
        self.prompt = prompt;
    }

    pub fn context(&mut self) -> &mut Context {
        self.context
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn show_autosuggestions(&self) -> bool { self.show_completions_hint.is_some() }

    // XXX: Returning a bool to indicate doneness is a bit awkward, maybe change it
    pub fn handle_newline(&mut self) -> io::Result<bool> {
        if self.is_search() {
            self.accept_autosuggestion()?;
        }
        self.clear_search();
        if self.show_completions_hint.is_some() {
            self.show_completions_hint = None;
            return Ok(false);
        }

        let char_before_cursor = cur_buf!(self).char_before(self.cursor);
        if char_before_cursor == Some('\\') {
            // self.insert_after_cursor('\r')?;
            self.insert_after_cursor('\n')?;
            Ok(false)
        } else {
            self.cursor = cur_buf!(self).num_chars();
            self.no_newline = true;
            self._display(false)?;
            self.out.write_all(b"\r\n")?;
            self.show_completions_hint = None;
            Ok(true)
        }
    }

    fn search_history_loc(&self) -> Option<usize> {
        if self.history_subset_index.len() > 0 {
            self.history_subset_loc.map(|i| self.history_subset_index[i])
        } else {
            None
        }
    }

    /// Refresh incremental search, either when started or when the buffer changes.
    fn refresh_search(&mut self, forward: bool) {
        let search_history_loc = self.search_history_loc();
        self.history_subset_index = self.context.history.search_index(&self.new_buf);
        if self.history_subset_index.len() > 0 {
            self.history_subset_loc = if forward {
                Some(0)
            } else {
                Some(self.history_subset_index.len() - 1)
            };
            if let Some(target_loc) = search_history_loc {
                for (i, history_loc) in self.history_subset_index.iter().enumerate() {
                    if target_loc <= *history_loc {
                        if forward || target_loc == *history_loc || i == 0 {
                            self.history_subset_loc = Some(i);
                        } else {
                            self.history_subset_loc = Some(i-1);
                        }
                        break;
                    }
                }

            }
        } else {
            self.history_subset_loc = None;
        }

        self.reverse_search = !forward;
        self.forward_search = forward;
        self.cur_history_loc = None;
        self.no_newline = true;
        self.buffer_changed = false;
    }

    /// Begin or continue a search through history.  If forward is true then start at top (or
    /// current_history_loc if set). If started with forward true then incremental search goes
    /// forward (top to bottom) other wise reverse (bottom to top).  It is valid to continue a
    /// search with forward changed (i.e. reverse search direction for one result).
    pub fn search(&mut self, forward: bool) -> io::Result<()> {
        if !self.is_search() {
            self.refresh_search(forward);
        } else if self.history_subset_index.len() > 0 {
            self.history_subset_loc = if let Some(p) = self.history_subset_loc {
                if forward {
                    if p < self.history_subset_index.len() - 1 { Some(p + 1) } else { Some(0) }
                } else {
                    if p > 0 { Some(p - 1) } else { Some(self.history_subset_index.len() - 1) }
                }
            } else {
                None
            };
        }
        self.display()?;
        Ok(())
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }

    /// Attempts to undo an action on the current buffer.
    ///
    /// Returns `Ok(true)` if an action was undone.
    /// Returns `Ok(false)` if there was no action to undo.
    pub fn undo(&mut self) -> io::Result<bool> {
        let did = cur_buf_mut!(self).undo();
        if did {
            self.move_cursor_to_end_of_line()?;
        } else {
            self.no_newline = true;
            self.display()?;
        }
        Ok(did)
    }

    pub fn redo(&mut self) -> io::Result<bool> {
        let did = cur_buf_mut!(self).redo();
        if did {
            self.move_cursor_to_end_of_line()?;
        } else {
            self.no_newline = true;
            self.display()?;
        }
        Ok(did)
    }

    pub fn revert(&mut self) -> io::Result<bool> {
        let did = cur_buf_mut!(self).revert();
        if did {
            self.move_cursor_to_end_of_line()?;
        } else {
            self.no_newline = true;
            self.display()?;
        }
        Ok(did)
    }

    fn print_completion_list(completions: &[String], highlighted: Option<usize>) -> io::Result<usize> {
        use std::cmp::max;

        let (w, _) = termion::terminal_size()?;

        // XXX wide character support
        let max_word_size = completions.iter().fold(1, |m, x| max(m, x.chars().count()));
        let cols = max(1, w as usize / (max_word_size));
        let col_width = 2 + w as usize / cols;
        let cols = max(1, w as usize / col_width);

        let mut lines = 0;

        BUFFER.with(|output_buf| {
            let mut i = 0;
            for (index, com) in completions.iter().enumerate() {
                if i == cols {
                    output_buf.append(b"\r\n");
                    lines += 1;
                    i = 0;
                } else if i > cols {
                    unreachable!()
                }

                if Some(index) == highlighted {
                    output_buf.append(color::Black.fg_str().as_bytes());
                    output_buf.append(color::White.bg_str().as_bytes());
                }
                write!(output_buf.0.borrow_mut(), "{:<1$}", com, col_width)?;
                if Some(index) == highlighted {
                    output_buf.append(color::Reset.bg_str().as_bytes());
                    output_buf.append(color::Reset.fg_str().as_bytes());
                }

                i += 1;
            }

            Ok(lines)
        })

    }

    pub fn skip_completions_hint(&mut self) {
        self.show_completions_hint = None;
    }

    pub fn complete(&mut self, handler: &mut EventHandler<W>, completion_type: CompleteType) -> io::Result<()> {
        handler(Event::new(self, EventKind::BeforeComplete));

        if let Some((completions, i)) = self.show_completions_hint.take() {

            let i = {

                use std::cmp::max;
                use std::cmp::min;

                let (w, _) = termion::terminal_size()?;

                // XXX wide character support
                let max_word_size = completions.iter().fold(1, |m, x| max(m, x.chars().count()));
                let cols_items = max(1, w as usize / (max_word_size));
                let col_width = 2 + w as usize / cols_items;
                let cols = max(1, w as usize / col_width);
                let line_count = (completions.len() as u64 / cols as u64) as usize;

                match i {
                    None => 0,
                    Some(i) => {
                        match completion_type {
                            CompleteType::Next => {
                                if i + 1 >= completions.len() {
                                    0
                                } else {
                                    min(i + 1, completions.len() - 1)
                                }
                            }
                            CompleteType::Prev => {
                                if i == 0 {
                                    completions.len() - 1
                                } else {
                                    max(i - 1, 0)
                                }
                            },
                            CompleteType::Up => {
                                if i + 1 < cols_items {
                                    i
                                } else {
                                   i + 1 - cols_items
                                }
                            },
                            CompleteType::Down => {
                                if i + cols_items - 1 > completions.len() - 1 {
                                    i
                                } else {
                                    i + cols_items - 1
                                }
                            },
                        }
                    }
                }
            };

            //let i = i.map_or(0, |i| (i+2) % completions.len());

            self.delete_word_before_cursor(false)?;
            self.insert_str_after_cursor(&completions[i])?;

            self.show_completions_hint = Some((completions, Some(i)));
        }
        if self.show_completions_hint.is_some() {
            self.no_newline = true;
            self.display()?;
            return Ok(());
        }

        let (word, completions) = {
            let word_range = self.get_word_before_cursor(false);
            let buf = cur_buf_mut!(self);

            let word = match word_range {
                Some((start, end)) => buf.range(start, end),
                None => "".into(),
            };

            if let Some(ref completer) = self.context.completer {
                let mut completions = completer.completions(word.as_ref());
                completions.sort();
                completions.dedup();
                (word, completions)
            } else {
                return Ok(());
            }
        };

        if completions.is_empty() {
            // Do nothing.
            self.show_completions_hint = None;
            Ok(())
        } else if completions.len() == 1 {
            self.show_completions_hint = None;
            self.delete_word_before_cursor(false)?;
            self.insert_str_after_cursor(completions[0].as_ref())
        } else {
            let common_prefix = util::find_longest_common_prefix(
                &completions
                    .iter()
                    .map(|x| x.chars().collect())
                    .collect::<Vec<Vec<char>>>()[..],
            );

            if let Some(p) = common_prefix {
                let s = p.iter().cloned().collect::<String>();

                if s.len() > word.len() && s.starts_with(&word[..]) {
                    self.delete_word_before_cursor(false)?;
                    return self.insert_str_after_cursor(s.as_ref());
                }
            }

            self.show_completions_hint = Some((completions, None));
            self.no_newline = true;
            self.display()?;

            Ok(())
        }
    }

    fn get_word_before_cursor(&self, ignore_space_before_cursor: bool) -> Option<(usize, usize)> {
        let (words, pos) = self.get_words_and_cursor_position();
        match pos {
            CursorPosition::InWord(i) => Some(words[i]),
            CursorPosition::InSpace(Some(i), _) => if ignore_space_before_cursor {
                Some(words[i])
            } else {
                None
            },
            CursorPosition::InSpace(None, _) => None,
            CursorPosition::OnWordLeftEdge(i) => if ignore_space_before_cursor && i > 0 {
                Some(words[i - 1])
            } else {
                None
            },
            CursorPosition::OnWordRightEdge(i) => Some(words[i]),
        }
    }

    /// Deletes the word preceding the cursor.
    /// If `ignore_space_before_cursor` is true and there is space directly before the cursor,
    /// this method ignores that space until it finds a word.
    /// If `ignore_space_before_cursor` is false and there is space directly before the cursor,
    /// nothing is deleted.
    pub fn delete_word_before_cursor(
        &mut self,
        ignore_space_before_cursor: bool,
    ) -> io::Result<()> {
        if let Some((start, _)) = self.get_word_before_cursor(ignore_space_before_cursor) {
            let moved = cur_buf_mut!(self).remove(start, self.cursor);
            self.cursor -= moved;
        }
        self.no_newline = true;
        self.display()
    }

    /// Clears the screen then prints the prompt and current buffer.
    pub fn clear(&mut self) -> io::Result<()> {
        BUFFER.with(|output_buf| {
            output_buf.append(clear::All.as_ref());
            output_buf.append(String::from(cursor::Goto(1,1)).as_bytes());
        });

        self.term_cursor_line = 1;
        self.no_newline = true;
        self.clear_search();
        self.display()
    }

    /// Move up (backwards) in history.
    pub fn move_up(&mut self) -> io::Result<()> {

        if self.show_autosuggestions() {
            return Ok(());
        }

        if self.is_search() {
            self.search(false)
        } else {
            if self.new_buf.num_chars() > 0 {
                match self.history_subset_loc {
                    Some(i) if i > 0 => {
                        self.history_subset_loc = Some(i - 1);
                        self.cur_history_loc = Some(self.history_subset_index[i - 1]);
                    }
                    None => {
                        self.history_subset_index = self.context.history.get_history_subset(&self.new_buf);
                        if self.history_subset_index.len() > 0 {
                            self.history_subset_loc = Some(self.history_subset_index.len() - 1);
                            self.cur_history_loc = Some(self.history_subset_index[self.history_subset_index.len() - 1]);
                        }
                    }
                    _ => ()
                }
            } else {
                match self.cur_history_loc {
                    Some(i) if i > 0 => self.cur_history_loc = Some(i - 1),
                    None if self.context.history.len() > 0  => self.cur_history_loc = Some(self.context.history.len()-1),
                    _ => ()
                }
            }
            self.move_cursor_to_end_of_line()
        }
    }

    /// Move down (forwards) in history, or to the new buffer if we reach the end of history.
    pub fn move_down(&mut self) -> io::Result<()> {

        if self.show_autosuggestions() {
            return Ok(());
        }

        if self.is_search() {
            self.search(true)
        } else {
            if self.new_buf.num_chars() > 0 {
                if let Some(i) = self.history_subset_loc {
                    if i < self.history_subset_index.len()-1 {
                        self.history_subset_loc = Some(i + 1);
                        self.cur_history_loc = Some(self.history_subset_index[i + 1]);
                    } else {
                        self.cur_history_loc = None;
                        self.history_subset_loc = None;
                        self.history_subset_index.clear();
                    }
                }
            } else {
                match self.cur_history_loc.take() {
                    Some(i) if i < self.context.history.len() - 1 => self.cur_history_loc = Some(i + 1),
                    _ => ()
                }
            }
            self.move_cursor_to_end_of_line()
        }
    }

    /// Moves to the start of history (ie. the earliest history entry).
    pub fn move_to_start_of_history(&mut self) -> io::Result<()> {
        if self.context.history.len() > 0 {
            self.cur_history_loc = Some(0);
            self.move_cursor_to_end_of_line()
        } else {
            self.cur_history_loc = None;
            self.no_newline = true;
            self.display()
        }
    }

    /// Moves to the end of history (ie. the new buffer).
    pub fn move_to_end_of_history(&mut self) -> io::Result<()> {
        if self.cur_history_loc.is_some() {
            self.cur_history_loc = None;
            self.move_cursor_to_end_of_line()
        } else {
            self.no_newline = true;
            self.display()
        }
    }

    /// Inserts a string directly after the cursor, moving the cursor to the right.
    ///
    /// Note: it is more efficient to call `insert_chars_after_cursor()` directly.
    pub fn insert_str_after_cursor(&mut self, s: &str) -> io::Result<()> {
        self.insert_chars_after_cursor(&s.chars().collect::<Vec<char>>()[..])
    }

    /// Inserts a character directly after the cursor, moving the cursor to the right.
    pub fn insert_after_cursor(&mut self, c: char) -> io::Result<()> {
        self.insert_chars_after_cursor(&[c])
    }

    /// Inserts characters directly after the cursor, moving the cursor to the right.
    pub fn insert_chars_after_cursor(&mut self, cs: &[char]) -> io::Result<()> {
        {
            let buf = cur_buf_mut!(self);
            buf.insert(self.cursor, cs);
        }

        self.cursor += cs.len();
        self.no_newline = true;
        self.display()
    }

    /// Deletes the character directly before the cursor, moving the cursor to the left.
    /// If the cursor is at the start of the line, nothing happens.
    pub fn delete_before_cursor(&mut self) -> io::Result<()> {
        if self.cursor > 0 {
            let buf = cur_buf_mut!(self);
            buf.remove(self.cursor - 1, self.cursor);
            self.cursor -= 1;
        }

        self.no_newline = true;
        self.display()
    }

    /// Deletes the character directly after the cursor. The cursor does not move.
    /// If the cursor is at the end of the line, nothing happens.
    pub fn delete_after_cursor(&mut self) -> io::Result<()> {
        {
            let buf = cur_buf_mut!(self);

            if self.cursor < buf.num_chars() {
                buf.remove(self.cursor, self.cursor + 1);
            }
        }
        self.no_newline = true;
        self.display()
    }

    /// Deletes every character preceding the cursor until the beginning of the line.
    pub fn delete_all_before_cursor(&mut self) -> io::Result<()> {
        cur_buf_mut!(self).remove(0, self.cursor);
        self.cursor = 0;
        self.no_newline = true;
        self.display()
    }

    /// Deletes every character after the cursor until the end of the line.
    pub fn delete_all_after_cursor(&mut self) -> io::Result<()> {
        {
            let buf = cur_buf_mut!(self);
            buf.truncate(self.cursor);
        }
        self.no_newline = true;
        self.display()
    }

    /// Deletes every character from the cursor until the given position.
    pub fn delete_until(&mut self, position: usize) -> io::Result<()> {
        {
            let buf = cur_buf_mut!(self);
            buf.remove(
                cmp::min(self.cursor, position),
                cmp::max(self.cursor, position),
            );
            self.cursor = cmp::min(self.cursor, position);
        }
        self.no_newline = true;
        self.display()
    }

    /// Deletes every character from the cursor until the given position, inclusive.
    pub fn delete_until_inclusive(&mut self, position: usize) -> io::Result<()> {
        {
            let buf = cur_buf_mut!(self);
            buf.remove(
                cmp::min(self.cursor, position),
                cmp::max(self.cursor + 1, position + 1),
            );
            self.cursor = cmp::min(self.cursor, position);
        }
        self.no_newline = true;
        self.display()
    }

    /// Moves the cursor to the left by `count` characters.
    /// The cursor will not go past the start of the buffer.
    pub fn move_cursor_left(&mut self, mut count: usize) -> io::Result<()> {

        if self.show_autosuggestions() {
           return self.display();
        }

        if count > self.cursor {
            count = self.cursor;
        }

        self.cursor -= count;

        self.no_newline = true;
        self.display()
    }

    /// Moves the cursor to the right by `count` characters.
    /// The cursor will not go past the end of the buffer.
    pub fn move_cursor_right(&mut self, mut count: usize) -> io::Result<()> {

        if self.show_autosuggestions() {
            return self.display();
        }

        {
            let buf = cur_buf!(self);

            if count > buf.num_chars() - self.cursor {
                count = buf.num_chars() - self.cursor;
            }

            self.cursor += count;
        }

        self.no_newline = true;
        self.display()
    }

    /// Moves the cursor to `pos`. If `pos` is past the end of the buffer, it will be clamped.
    pub fn move_cursor_to(&mut self, pos: usize) -> io::Result<()> {
        self.cursor = pos;
        let buf_len = cur_buf!(self).num_chars();
        if self.cursor > buf_len {
            self.cursor = buf_len;
        }
        self.no_newline = true;
        self.display()
    }

    /// Moves the cursor to the start of the line.
    pub fn move_cursor_to_start_of_line(&mut self) -> io::Result<()> {
        self.cursor = 0;
        self.no_newline = true;
        self.display()
    }

    /// Moves the cursor to the end of the line.
    pub fn move_cursor_to_end_of_line(&mut self) -> io::Result<()> {
        //self.clear_search();
        self.cursor = cur_buf!(self).num_chars();
        self.no_newline = true;
        self.display()
    }

    pub fn cursor_is_at_end_of_line(&self) -> bool {
        let num_chars = cur_buf!(self).num_chars();
        if self.no_eol {
            self.cursor == num_chars - 1
        } else {
            self.cursor == num_chars
        }
    }

    ///  Returns a reference to the current buffer being edited.
    /// This may be the new buffer or a buffer from history.
    pub fn current_buffer(&self) -> &Buffer {
        cur_buf!(self)
    }

    ///  Returns a mutable reference to the current buffer being edited.
    /// This may be the new buffer or a buffer from history.
    pub fn current_buffer_mut(&mut self) -> &mut Buffer {
        cur_buf_mut!(self)
    }

    /// Accept autosuggestion and copy its content into current buffer
    pub fn accept_autosuggestion(&mut self) -> io::Result<()> {
        if self.show_autosuggestions {
            {
                let autosuggestion = self.autosuggestion.clone();
                let search = self.is_search();
                let buf = self.current_buffer_mut();
                match autosuggestion {
                    Some(ref x) if search => buf.copy_buffer(x),
                    Some(ref x) => buf.insert_from_buffer(x),
                    None => ()
                }
            }
        }
        self.clear_search();
        self.move_cursor_to_end_of_line()
    }

    /// Returns current auto suggestion, for history search this is the current match if not
    /// searching the first history entry to start with current text (reverse order).
    /// Return None if nothing found.
    fn current_autosuggestion(&mut self) -> Option<Buffer> {
        let context_history = &self.context.history;
        let autosuggestion = if self.is_search() {
            self.search_history_loc().map(|i| &context_history[i])
        } else if self.show_autosuggestions {
            self.cur_history_loc
                .map(|i| &context_history[i])
                .or_else(|| {
                    context_history
                        .get_newest_match(Some(context_history.len()), &self.new_buf)
                        .map(|i| &context_history[i])
                })
        } else {
            None
        };
        autosuggestion.cloned()
    }

    pub fn is_currently_showing_autosuggestion(&self) -> bool {
        self.autosuggestion.is_some()
    }

    /// Override the prompt for incremental search if needed.
    fn search_prompt(&mut self) -> (String, usize) {
        if self.is_search() {
            // If we are searching override prompt to search prompt.
            let (hplace, color) = if self.history_subset_index.len() > 0 {
                (self.history_subset_loc.unwrap_or(0) + 1, color::Green.fg_str())
            } else {
                (0, color::Red.fg_str())
            };
            (format!("(search)'{}{}{}` ({}/{}): ",
                     color, self.current_buffer(), color::Reset.fg_str(),
                     hplace,
                     self.history_subset_index.len()),
             9)
        } else {
            (self.prompt.clone(), 0)
        }
    }

    fn _display(&mut self, show_autosuggest: bool) -> io::Result<()> {
        BUFFER.with(|output_buf| {
            fn calc_width(prompt_width: usize, buf_widths: &[usize], terminal_width: usize) -> usize {
                let mut total = 0;

                for line in buf_widths {
                    if total % terminal_width != 0 {
                        total = ((total / terminal_width) + 1) * terminal_width;
                    }

                    total += prompt_width + line;
                }

                total
            }

            let (prompt, rev_prompt_width) = self.search_prompt();

            let terminal_width = util::terminal_width()?;
            let prompt_width = util::last_prompt_line_width(&prompt);

            let buf = cur_buf!(self);
            let buf_width = buf.width();

            // Don't let the cursor go over the end!
            let buf_num_chars = buf.num_chars();
            if buf_num_chars < self.cursor {
                self.cursor = buf_num_chars;
            }

            // Can't move past the last character in vi normal mode
            if self.no_eol && self.cursor != 0 && self.cursor == buf_num_chars {
                self.cursor -= 1;
            }

            let buf_widths = match self.autosuggestion {
                Some(ref suggestion) => suggestion.width(),
                None => buf_width,
            };
            // Width of the current buffer lines (including autosuggestion) from the start to the cursor
            let buf_widths_to_cursor = match self.autosuggestion {
                Some(ref suggestion) =>
                    // Cursor might overrun autosuggestion with history search.
                    if self.cursor < suggestion.num_chars() {
                        suggestion.range_width(0, self.cursor)
                    } else {
                        buf.range_width(0, self.cursor)
                    },
                None => buf.range_width(0, self.cursor)
            };

            // Total number of terminal spaces taken up by prompt and buffer
            let new_total_width = calc_width(prompt_width, &buf_widths, terminal_width);
            let new_total_width_to_cursor = if self.is_search() {
                calc_width(rev_prompt_width, &buf_widths_to_cursor, terminal_width)
            } else {
                calc_width(prompt_width, &buf_widths_to_cursor, terminal_width)
            };

            let new_num_lines = (new_total_width + terminal_width) / terminal_width;

            // Move the term cursor to the same line as the prompt.
            if self.term_cursor_line > 1 {
                output_buf.append(cursor::Up(self.term_cursor_line as u16 - 1).to_string().as_bytes());
            }

            if ! self.no_newline {
                output_buf.append("⏎".as_bytes());
                for _ in 0..(terminal_width - 1) {
                    output_buf.push(b' ');
                }
            }

            output_buf.push(b'\r');
            output_buf.append(clear::AfterCursor.as_ref());

            // If we're cycling through completions, show those
            let mut completion_lines = 0;
            if let Some((completions, i)) = self.show_completions_hint.as_ref() {
                completion_lines = 1 + Self::print_completion_list(completions, *i)?;
                output_buf.append(b"\r\n");
            }

            // Write the prompt
            if ! self.no_newline {
                for line in prompt.split('\n') {
                    output_buf.append(line.as_bytes());
                    output_buf.append(b"\r\n");
                }
                output_buf.pop(); // pop the '\n'
                output_buf.pop(); // pop the '\r'
            } else {
                output_buf.append(util::handle_prompt(&prompt).as_bytes());
            }

            // If we have an autosuggestion, we make the autosuggestion the buffer we print out.
            // We get the number of bytes in the buffer (but NOT the autosuggestion).
            // Then, we loop and subtract from that number until it's 0, in which case we are printing
            // the autosuggestion from here on (in a different color).
            let lines = if show_autosuggest {
                match self.autosuggestion {
                    Some(ref suggestion) => suggestion.lines(),
                    None => buf.lines(),
                }
            } else {
                buf.lines()
            };
            let mut buf_num_remaining_bytes = buf.num_bytes();

            let lines_len = lines.len();
            for (i, line) in lines.into_iter().enumerate() {
                if i > 0 {
                    output_buf.append(cursor::Right(prompt_width as u16).to_string().as_bytes());
                }

                if buf_num_remaining_bytes == 0 {
                    output_buf.append(line.as_bytes());
                } else if line.len() > buf_num_remaining_bytes {
                    let start = &line[..buf_num_remaining_bytes];
                    let start = match self.closure {
                        Some(ref f) => f(start),
                        None => start.to_owned(),
                    };
                    if self.is_search() {
                        output_buf.append(color::Yellow.fg_str().as_bytes());
                    }
                    output_buf.append(start.as_bytes());
                    if !self.is_search() {
                        output_buf.append(color::Yellow.fg_str().as_bytes());
                    }
                    output_buf.append(line[buf_num_remaining_bytes..].as_bytes());
                    buf_num_remaining_bytes = 0;
                } else {
                    buf_num_remaining_bytes -= line.len();
                    let written_line = match self.closure {
                        Some(ref f) => f(&line),
                        None => line,
                    };
                    if self.is_search() {
                        output_buf.append(color::Yellow.fg_str().as_bytes());
                    }
                    output_buf.append(written_line.as_bytes());
                }

                if i + 1 < lines_len {
                    output_buf.append(b"\r\n");
                }
            }

            if self.is_currently_showing_autosuggestion() || self.is_search() {
                output_buf.append(color::Reset.fg_str().as_bytes());
            }

            // at the end of the line, move the cursor down a line
            if new_total_width % terminal_width == 0 {
                output_buf.append(b"\r\n");
            }

            self.term_cursor_line = (new_total_width_to_cursor + terminal_width) / terminal_width;

            // The term cursor is now on the bottom line. We may need to move the term cursor up
            // to the line where the true cursor is.
            let cursor_line_diff = new_num_lines as isize - self.term_cursor_line as isize;
            if cursor_line_diff > 0 {
                output_buf.append(cursor::Up(cursor_line_diff as u16).to_string().as_bytes());
            } else if cursor_line_diff < 0 {
                unreachable!();
            }

            // Now that we are on the right line, we must move the term cursor left or right
            // to match the true cursor.
            let cursor_col_diff = new_total_width as isize - new_total_width_to_cursor as isize -
                cursor_line_diff * terminal_width as isize;
            if cursor_col_diff > 0 {
                output_buf.append(cursor::Left(cursor_col_diff as u16).to_string().as_bytes());
            } else if cursor_col_diff < 0 {
                output_buf.append(cursor::Right((-cursor_col_diff) as u16).to_string().as_bytes());
            }

            self.term_cursor_line += completion_lines;

            {
                let out = &mut self.out;
                output_buf.extract(|b| out.write_all(b))?;
                out.flush()
            }
        })
    }

    /// Deletes the displayed prompt and buffer, replacing them with the current prompt and buffer
    pub fn display(&mut self) -> io::Result<()> {
        if self.is_search() && self.buffer_changed {
            // Refresh incremental search.
            let forward = self.forward_search;
            self.refresh_search(forward);
        }
        self.autosuggestion = self.current_autosuggestion();

        self._display(true)
    }
}

impl<'a, W: Write> From<Editor<'a, W>> for String {
    fn from(ed: Editor<'a, W>) -> String {
        match ed.cur_history_loc {
            Some(i) => ed.context.history[i].clone(),
            _ => ed.new_buf,
        }.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Context;

    #[test]
    /// test undoing delete_all_after_cursor
    fn delete_all_after_cursor_undo() {
        let mut context = Context::new();
        let out = Vec::new();
        let mut ed = Editor::new(out, "prompt".to_owned(), None, &mut context).unwrap();
        ed.insert_str_after_cursor("delete all of this").unwrap();
        ed.move_cursor_to_start_of_line().unwrap();
        ed.delete_all_after_cursor().unwrap();
        ed.undo().unwrap();
        assert_eq!(String::from(ed), "delete all of this");
    }

    #[test]
    fn move_cursor_left() {
        let mut context = Context::new();
        let out = Vec::new();
        let mut ed = Editor::new(out, "prompt".to_owned(), None, &mut context).unwrap();
        ed.insert_str_after_cursor("let").unwrap();
        assert_eq!(ed.cursor, 3);

        ed.move_cursor_left(1).unwrap();
        assert_eq!(ed.cursor, 2);

        ed.insert_after_cursor('f').unwrap();
        assert_eq!(ed.cursor, 3);
        assert_eq!(String::from(ed), "left");
    }

    #[test]
    fn cursor_movement() {
        let mut context = Context::new();
        let out = Vec::new();
        let mut ed = Editor::new(out, "prompt".to_owned(), None, &mut context).unwrap();
        ed.insert_str_after_cursor("right").unwrap();
        assert_eq!(ed.cursor, 5);

        ed.move_cursor_left(2).unwrap();
        ed.move_cursor_right(1).unwrap();
        assert_eq!(ed.cursor, 4);
    }

    #[test]
    fn delete_until_backwards() {
        let mut context = Context::new();
        let out = Vec::new();
        let mut ed = Editor::new(out, "prompt".to_owned(), None, &mut context).unwrap();
        ed.insert_str_after_cursor("right").unwrap();
        assert_eq!(ed.cursor, 5);

        ed.delete_until(0).unwrap();
        assert_eq!(ed.cursor, 0);
        assert_eq!(String::from(ed), "");
    }

    #[test]
    fn delete_until_forwards() {
        let mut context = Context::new();
        let out = Vec::new();
        let mut ed = Editor::new(out, "prompt".to_owned(), None, &mut context).unwrap();
        ed.insert_str_after_cursor("right").unwrap();
        ed.cursor = 0;

        ed.delete_until(5).unwrap();
        assert_eq!(ed.cursor, 0);
        assert_eq!(String::from(ed), "");
    }

    #[test]
    fn delete_until() {
        let mut context = Context::new();
        let out = Vec::new();
        let mut ed = Editor::new(out, "prompt".to_owned(), None, &mut context).unwrap();
        ed.insert_str_after_cursor("right").unwrap();
        ed.cursor = 4;

        ed.delete_until(1).unwrap();
        assert_eq!(ed.cursor, 1);
        assert_eq!(String::from(ed), "rt");
    }

    #[test]
    fn delete_until_inclusive() {
        let mut context = Context::new();
        let out = Vec::new();
        let mut ed = Editor::new(out, "prompt".to_owned(), None, &mut context).unwrap();
        ed.insert_str_after_cursor("right").unwrap();
        ed.cursor = 4;

        ed.delete_until_inclusive(1).unwrap();
        assert_eq!(ed.cursor, 1);
        assert_eq!(String::from(ed), "r");
    }
}

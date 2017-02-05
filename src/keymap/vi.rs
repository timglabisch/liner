use std::{mem, cmp};
use std::io::{self, Write};
use termion::event::Key;

use KeyMap;
use Editor;

/// The editing mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Insert,
    Normal,
    Replace,
    Delete(usize),
}

struct ModeStack(Vec<Mode>);

impl ModeStack {
    fn with_insert() -> Self {
        ModeStack(vec![Mode::Insert])
    }

    /// Get the current mode.
    ///
    /// If the stack is empty, we are in normal mode.
    fn mode(&self) -> Mode {
        self.0.last()
            .map(|&m| m)
            .unwrap_or(Mode::Normal)
    }

    /// Empty the stack and return to normal mode.
    fn clear(&mut self) {
        self.0.clear()
    }

    /// Push the given mode on to the stack.
    fn push(&mut self, m: Mode) {
        self.0.push(m)
    }

    fn pop(&mut self) -> Mode {
        self.0.pop()
            .unwrap_or(Mode::Normal)
    }
}

fn is_movement_key(key: Key) -> bool {
    match key {
        Key::Char('h') | Key::Char('l') | Key::Left | Key::Right |
            Key::Backspace | Key::Char(' ') | Key::Home | Key::End |
            Key::Char('$')
        => true,
        _ => false,
    }
}

pub struct Vi<'a, W: Write> {
    ed: Editor<'a, W>,
    mode_stack: ModeStack,
    current_command: Vec<Key>,
    last_command: Vec<Key>,
    current_insert: Option<Key>,
    last_insert: Option<Key>,
    count: u32,
    secondary_count: u32,
    last_count: u32,
    movement_reset: bool,
}

impl<'a, W: Write> Vi<'a, W> {
    pub fn new(mut ed: Editor<'a, W>) -> Self {
        // since we start in insert mode, we need to start an undo group
        ed.current_buffer_mut().start_undo_group();

        Vi {
            ed: ed,
            mode_stack: ModeStack::with_insert(),
            current_command: Vec::new(),
            last_command: Vec::new(),
            current_insert: None,
            // we start vi in insert mode
            last_insert: Some(Key::Char('i')),
            count: 0,
            secondary_count: 0,
            last_count: 0,
            movement_reset: false,
        }
    }

    /// Get the current mode.
    fn mode(&self) -> Mode {
        self.mode_stack.mode()
    }

    fn set_mode(&mut self, mode: Mode) {
        use self::Mode::*;
        self.set_mode_preserve_last(mode);
        if mode == Insert {
            self.last_count = 0;
            self.last_command.clear();
        }
    }

    fn set_mode_preserve_last(&mut self, mode: Mode) {
        use self::Mode::*;

        self.ed.no_eol = mode == Normal;
        self.movement_reset = mode != Insert;
        self.mode_stack.push(mode);

        if mode == Insert {
            self.ed.current_buffer_mut().start_undo_group();
        }
    }

    fn pop_mode_after_movement(&mut self) -> io::Result<()> {
        use self::Mode::*;

        let original_mode = self.mode_stack.pop();

        self.ed.no_eol = self.mode() == Mode::Normal;
        self.movement_reset = self.mode() != Mode::Insert;

        match original_mode {
            Delete(start_pos) => {
                // perform the delete operation
                try!(self.ed.delete_until(start_pos));

                // update the last state
                mem::swap(&mut self.last_command, &mut self.current_command);
                self.last_insert = self.current_insert;
                self.last_count = self.count;

                // reset our counts
                self.count = 0;
                self.secondary_count = 0;
            }
            // in normal mode, count goes back to 0 after movement
            Normal => {
                self.count = 0;
            }
            _ => {}
        };

        Ok(())
    }

    fn pop_mode(&mut self) {
        use self::Mode::*;

        let last_mode = self.mode_stack.pop();
        self.ed.no_eol = self.mode() == Normal;
        self.movement_reset = self.mode() != Insert;

        if last_mode == Insert {
            self.ed.current_buffer_mut().end_undo_group();
        }
    }

    /// Return to normal mode.
    fn normal_mode_abort(&mut self) {
        self.mode_stack.clear();
        self.ed.no_eol = true;
        self.count = 0;
    }

    /// When doing a move, 0 should behave the same as 1 as far as the count goes.
    fn move_count(&mut self) -> usize {
        match self.count {
            0 => 1,
            _ => self.count as usize,
        }
    }

    /// Get the current count or the number of remaining chars in the buffer.
    fn move_count_left(&mut self) -> usize {
        cmp::min(self.ed.cursor(), self.move_count())
    }

    /// Get the current count or the number of remaining chars in the buffer.
    fn move_count_right(&mut self) -> usize {
        cmp::min(self.ed.current_buffer().num_chars() - self.ed.cursor(), self.move_count())
    }

    fn repeat(&mut self) -> io::Result<()> {
        self.last_count = self.count;
        let keys = mem::replace(&mut self.last_command, Vec::new());

        if let Some(insert_key) = self.last_insert {
            // enter insert mode if necessary
            try!(self.handle_key_core(insert_key));
        }

        for k in keys.iter() {
            try!(self.handle_key_core(*k));
        }

        if self.last_insert.is_some() {
            // leave insert mode
            try!(self.handle_key_core(Key::Esc));
        }

        // restore the last command
        mem::replace(&mut self.last_command, keys);

        Ok(())
    }

    fn handle_key_common(&mut self, key: Key) -> io::Result<()> {
        match key {
            Key::Ctrl('l') => self.ed.clear(),
            Key::Left => self.ed.move_cursor_left(1),
            Key::Right => self.ed.move_cursor_right(1),
            Key::Up => self.ed.move_up(),
            Key::Down => self.ed.move_down(),
            Key::Home => self.ed.move_cursor_to_start_of_line(),
            Key::End => self.ed.move_cursor_to_end_of_line(),
            Key::Backspace => self.ed.delete_before_cursor(),
            Key::Delete => self.ed.delete_after_cursor(),
            Key::Null => Ok(()),
            _ => Ok(()),
        }
    }

    fn handle_key_insert(&mut self, key: Key) -> io::Result<()> {
        match key {
            Key::Esc => {
                // perform any repeats
                if self.count > 0 {
                    self.last_count = self.count;
                    for _ in 1..self.count {
                        let keys = mem::replace(&mut self.last_command, Vec::new());
                        for k in keys.into_iter() {
                            try!(self.handle_key_core(k));
                        }
                    }
                    self.count = 0;
                }
                // cursor moves to the left when switching from insert to normal mode
                try!(self.ed.move_cursor_left(1));
                self.pop_mode();
                Ok(())
            }
            Key::Char(c) => {
                if self.movement_reset {
                    self.ed.current_buffer_mut().end_undo_group();
                    self.ed.current_buffer_mut().start_undo_group();
                    self.last_command.clear();
                    self.movement_reset = false;
                    // vim behaves as if this was 'i'
                    self.last_insert = Some(Key::Char('i'));
                }
                self.last_command.push(key);
                self.ed.insert_after_cursor(c)
            }
            // delete and backspace need to be included in the command buffer
            Key::Backspace | Key::Delete => {
                if self.movement_reset {
                    self.ed.current_buffer_mut().end_undo_group();
                    self.ed.current_buffer_mut().start_undo_group();
                    self.last_command.clear();
                    self.movement_reset = false;
                    // vim behaves as if this was 'i'
                    self.last_insert = Some(Key::Char('i'));
                }
                self.last_command.push(key);
                self.handle_key_common(key)
            }
            // if this is a movement while in insert mode, reset the repeat count
            Key::Left | Key::Right | Key::Home | Key::End => {
                self.count = 0;
                self.movement_reset = true;
                self.handle_key_common(key)
            }
            // up and down require even more special handling
            Key::Up => {
                self.count = 0;
                self.movement_reset = true;
                self.ed.current_buffer_mut().end_undo_group();
                try!(self.ed.move_up());
                self.ed.current_buffer_mut().start_undo_group();
                Ok(())
            }
            Key::Down => {
                self.count = 0;
                self.movement_reset = true;
                self.ed.current_buffer_mut().end_undo_group();
                try!(self.ed.move_down());
                self.ed.current_buffer_mut().start_undo_group();
                Ok(())
            }
            _ => self.handle_key_common(key),
        }
    }

    fn handle_key_normal(&mut self, key: Key) -> io::Result<()> {
        use self::Mode::*;

        match key {
            Key::Esc => {
                self.count = 0;
                Ok(())
            }
            Key::Char('i') => {
                self.last_insert = Some(key);
                self.set_mode(Insert);
                Ok(())
            }
            Key::Char('a') => {
                self.last_insert = Some(key);
                self.set_mode(Insert);
                self.ed.move_cursor_right(1)
            }
            Key::Char('A') => {
                self.last_insert = Some(key);
                self.set_mode(Insert);
                self.ed.move_cursor_to_end_of_line()
            }
            Key::Char('I') => {
                self.last_insert = Some(key);
                self.set_mode(Insert);
                self.ed.move_cursor_to_start_of_line()
            }
            Key::Char('s') => {
                self.last_insert = Some(key);
                self.set_mode(Insert);
                let pos = self.ed.cursor() + self.move_count_right();
                try!(self.ed.delete_until(pos));
                self.last_count = self.count;
                self.count = 0;
                Ok(())
            }
            Key::Char('r') => {
                self.set_mode(Mode::Replace);
                Ok(())
            }
            Key::Char('d') | Key::Char('c') => {
                self.current_command.clear();

                if key == Key::Char('d') {
                    // handle special 'd' key stuff
                    self.current_insert = None;
                    self.current_command.push(key);
                }
                else {
                    // handle special 'c' key stuff
                    self.current_insert = Some(key);
                    self.current_command.clear();
                    self.set_mode(Insert);
                }

                let start_pos = self.ed.cursor();
                self.set_mode(Mode::Delete(start_pos));
                self.secondary_count = self.count;
                self.count = 0;
                Ok(())
            }
            Key::Char('D') => {
                // update the last command state
                self.last_insert = None;
                self.last_command.clear();
                self.last_command.push(key);
                self.count = 0;
                self.last_count = 0;

                self.ed.delete_all_after_cursor()
            }
            Key::Char('C') => {
                // update the last command state
                self.last_insert = None;
                self.last_command.clear();
                self.last_command.push(key);
                self.count = 0;
                self.last_count = 0;

                self.set_mode_preserve_last(Insert);
                self.ed.delete_all_after_cursor()
            }
            Key::Char('.') => {
                // repeat the last command
                self.count = match (self.count, self.last_count) {
                    // if both count and last_count are zero, use 1
                    (0, 0) => 1,
                    // if count is zero, use last_count
                    (0, _) => self.last_count,
                    // otherwise use count
                    (_, _) => self.count,
                };
                self.repeat()
            }
            Key::Char('h') | Key::Left | Key::Backspace => {
                let count = self.move_count_left();
                try!(self.ed.move_cursor_left(count));
                self.pop_mode_after_movement()
            }
            Key::Char('l') | Key::Right | Key::Char(' ') => {
                let count = self.move_count_right();
                try!(self.ed.move_cursor_right(count));
                self.pop_mode_after_movement()
            }
            Key::Char('k') | Key::Up =>  {
                try!(self.ed.move_up());
                self.pop_mode_after_movement()
            }
            Key::Char('j') | Key::Down => {
                try!(self.ed.move_down());
                self.pop_mode_after_movement()
            }
            // if count is 0, 0 should move to start of line
            Key::Char('0') if self.count == 0 => {
                try!(self.ed.move_cursor_to_start_of_line());
                self.pop_mode_after_movement()
            }
            Key::Char(i @ '0'...'9') => {
                let i = i.to_digit(10).unwrap();
                // count = count * 10 + i
                self.count = self.count
                    .saturating_mul(10)
                    .saturating_add(i);
                Ok(())
            }
            Key::Char('$') => {
                try!(self.ed.move_cursor_to_end_of_line());
                self.pop_mode_after_movement()
            }
            Key::Char('x') | Key::Delete => {
                // update the last command state
                self.last_insert = None;
                self.last_command.clear();
                self.last_command.push(key);
                self.last_count = self.count;

                let pos = self.ed.cursor() + self.move_count_right();
                try!(self.ed.delete_until(pos));
                self.count = 0;
                Ok(())
            }
            Key::Char('u') => {
                let count = self.move_count();
                self.count = 0;
                for _ in 0..count {
                    let did = try!(self.ed.undo());
                    if !did {
                        break;
                    }
                }
                Ok(())
            }
            Key::Ctrl('r') => {
                let count = self.move_count();
                self.count = 0;
                for _ in 0..count {
                    let did = try!(self.ed.redo());
                    if !did {
                        break;
                    }
                }
                Ok(())
            }
            _ => self.handle_key_common(key),
        }
    }

    fn handle_key_replace(&mut self, key: Key) -> io::Result<()> {
        match key {
            Key::Char(c) => {
                // make sure there are enough chars to replace
                if self.move_count_right() == self.move_count() {
                    // update the last command state
                    self.last_insert = None;
                    self.last_command.clear();
                    self.last_command.push(Key::Char('r'));
                    self.last_command.push(key);
                    self.last_count = self.count;

                    // replace count characters
                    self.ed.current_buffer_mut().start_undo_group();
                    for _ in 0..self.move_count_right() {
                        try!(self.ed.delete_after_cursor());
                        try!(self.ed.insert_after_cursor(c));
                    }
                    self.ed.current_buffer_mut().end_undo_group();

                    try!(self.ed.move_cursor_left(1));
                }
                self.pop_mode();
            }
            // not a char
            _ => {
                self.normal_mode_abort();
            }
        };

        // back to normal mode
        self.count = 0;
        Ok(())
    }

    fn handle_key_delete_or_change(&mut self, key: Key) -> io::Result<()> {
        match (key, self.current_insert) {
            // check if this is a movement key
            (key, _) if is_movement_key(key) | (key == Key::Char('0') && self.count == 0) => {
                // set count
                self.count = match (self.count, self.secondary_count) {
                    (0, 0) => 0,
                    (_, 0) => self.count,
                    (0, _) => self.secondary_count,
                    _ => {
                        // secondary_count * count
                        self.secondary_count
                            .saturating_mul(self.count)
                    }
                };

                // update the last command state
                self.current_command.push(key);

                // execute movement
                self.handle_key_normal(key)
            }
            // handle numeric keys
            (Key::Char('0'...'9'), _) => {
                self.handle_key_normal(key)
            }
            (Key::Char('c'), Some(Key::Char('c'))) | (Key::Char('d'), None) => {
                // updating the last command buffer doesn't really make sense in this context.
                // Repeating 'dd' will simply erase and already erased line. Any other commands
                // will then become the new last command and the user will need to press 'dd' again
                // to clear the line. The same largely applies to the 'cc' command. We update the
                // last command here anyway ¯\_(ツ)_/¯
                self.current_command.push(key);

                // delete the whole line
                self.count = 0;
                self.secondary_count = 0;
                try!(self.ed.move_cursor_to_start_of_line());
                try!(self.ed.delete_all_after_cursor());

                // return to the previous mode
                self.pop_mode();
                Ok(())
            }
            // not a delete or change command, back to normal mode
            _ => {
                self.normal_mode_abort();
                Ok(())
            }
        }
    }
}

impl<'a, W: Write> KeyMap<'a, W, Vi<'a, W>> for Vi<'a, W> {
    fn handle_key_core(&mut self, key: Key) -> io::Result<()> {
        match self.mode() {
            Mode::Normal => self.handle_key_normal(key),
            Mode::Insert => self.handle_key_insert(key),
            Mode::Replace => self.handle_key_replace(key),
            Mode::Delete(_) => self.handle_key_delete_or_change(key),
        }
    }

    fn editor(&mut self) ->  &mut Editor<'a, W> {
        &mut self.ed
    }
}

impl<'a, W: Write> From<Vi<'a, W>> for String {
    fn from(vi: Vi<'a, W>) -> String {
        vi.ed.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use termion::event::Key;
    use termion::event::Key::*;
    use Buffer;
    use Context;
    use Editor;
    use KeyMap;
    use std::io::Write;

    macro_rules! simulate_keys {
        ($keymap:ident, $keys:expr) => {{
            simulate_keys(&mut $keymap, $keys.into_iter())
        }}
    }

    fn simulate_keys<'a, 'b, W: Write, T, M: KeyMap<'a, W, T>, I>(keymap: &mut M, keys: I) -> bool
        where I: Iterator<Item=&'b Key>
    {
        for k in keys {
            if keymap.handle_key(*k, &mut |_| {}).unwrap() {
                return true;
            }
        }

        false
    }

    #[test]
    fn enter_is_done() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("done").unwrap();
        assert_eq!(map.ed.cursor(), 4);

        assert!(simulate_keys!(map, [
            Char('\n'),
        ]));

        assert_eq!(map.ed.cursor(), 4);
        assert_eq!(String::from(map), "done");
    }

    #[test]
    fn move_cursor_left() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.editor().insert_str_after_cursor("let").unwrap();
        assert_eq!(map.ed.cursor(), 3);

        simulate_keys!(map, [
            Left,
            Char('f'),
        ]);

        assert_eq!(map.ed.cursor(), 3);
        assert_eq!(String::from(map), "left");
    }

    #[test]
    fn cursor_movement() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("right").unwrap();
        assert_eq!(map.ed.cursor(), 5);

        simulate_keys!(map, [
            Left,
            Left,
            Right,
        ]);

        assert_eq!(map.ed.cursor(), 4);
    }

    #[test]
    fn vi_initial_insert() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);

        simulate_keys!(map, [
            Char('i'),
            Char('n'),
            Char('s'),
            Char('e'),
            Char('r'),
            Char('t'),
        ]);

        assert_eq!(map.ed.cursor(), 6);
        assert_eq!(String::from(map), "insert");
    }

    #[test]
    fn vi_left_right_movement() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("data").unwrap();
        assert_eq!(map.ed.cursor(), 4);

        simulate_keys!(map, [Left]);
        assert_eq!(map.ed.cursor(), 3);
        simulate_keys!(map, [Right]);
        assert_eq!(map.ed.cursor(), 4);

        // switching from insert mode moves the cursor left
        simulate_keys!(map, [Esc, Left]);
        assert_eq!(map.ed.cursor(), 2);
        simulate_keys!(map, [Right]);
        assert_eq!(map.ed.cursor(), 3);

        simulate_keys!(map, [Char('h')]);
        assert_eq!(map.ed.cursor(), 2);
        simulate_keys!(map, [Char('l')]);
        assert_eq!(map.ed.cursor(), 3);
    }

    #[test]
    /// Shouldn't be able to move past the last char in vi normal mode
    fn vi_no_eol() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("data").unwrap();
        assert_eq!(map.ed.cursor(), 4);

        simulate_keys!(map, [Esc]);
        assert_eq!(map.ed.cursor(), 3);

        simulate_keys!(map, [Right, Right]);
        assert_eq!(map.ed.cursor(), 3);

        // in insert mode, we can move past the last char, but no further
        simulate_keys!(map, [Char('i'), Right, Right]);
        assert_eq!(map.ed.cursor(), 4);
    }

    #[test]
    /// Cursor moves left when exiting insert mode.
    fn vi_switch_from_insert() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("data").unwrap();
        assert_eq!(map.ed.cursor(), 4);

        simulate_keys!(map, [Esc]);
        assert_eq!(map.ed.cursor(), 3);

        simulate_keys!(map, [
            Char('i'),
            Esc,
            Char('i'),
            Esc,
            Char('i'),
            Esc,
            Char('i'),
            Esc,
        ]);
        assert_eq!(map.ed.cursor(), 0);
    }

    #[test]
    fn vi_normal_history_cursor_eol() {
        let mut context = Context::new();
        context.history.push("history".into()).unwrap();
        context.history.push("history".into()).unwrap();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("data").unwrap();
        assert_eq!(map.ed.cursor(), 4);

        simulate_keys!(map, [Up]);
        assert_eq!(map.ed.cursor(), 7);

        // in normal mode, make sure we don't end up past the last char
        simulate_keys!(map, [Esc, Up]);
        assert_eq!(map.ed.cursor(), 6);
    }

    #[test]
    fn vi_normal_delete() {
        let mut context = Context::new();
        context.history.push("history".into()).unwrap();
        context.history.push("history".into()).unwrap();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("data").unwrap();
        assert_eq!(map.ed.cursor(), 4);

        simulate_keys!(map, [
            Esc,
            Char('0'),
            Delete,
            Char('x'),
        ]);
        assert_eq!(map.ed.cursor(), 0);
        assert_eq!(String::from(map), "ta");
    }

    #[test]
    fn vi_substitute_command() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("data").unwrap();
        assert_eq!(map.ed.cursor(), 4);

        simulate_keys!(map, [
            Esc,
            Char('0'),
            Char('s'),
            Char('s'),
        ]);
        assert_eq!(String::from(map), "sata");
    }

    #[test]
    fn substitute_with_count() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("data").unwrap();
        assert_eq!(map.ed.cursor(), 4);

        simulate_keys!(map, [
            Esc,
            Char('0'),
            Char('2'),
            Char('s'),
            Char('b'),
            Char('e'),
        ]);
        assert_eq!(String::from(map), "beta");
    }

    #[test]
    fn substitute_with_count_repeat() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("data data").unwrap();

        simulate_keys!(map, [
            Esc,
            Char('0'),
            Char('2'),
            Char('s'),
            Char('b'),
            Char('e'),
            Esc,
            Char('4'),
            Char('l'),
            Char('.'),
        ]);
        assert_eq!(String::from(map), "beta beta");
    }

    #[test]
    /// make sure our count is accurate
    fn vi_count() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);

        simulate_keys!(map, [
            Esc,
        ]);
        assert_eq!(map.count, 0);

        simulate_keys!(map, [
            Char('1'),
        ]);
        assert_eq!(map.count, 1);

        simulate_keys!(map, [
            Char('1'),
        ]);
        assert_eq!(map.count, 11);

        // switching to insert mode and back to edit mode should reset the count
        simulate_keys!(map, [
            Char('i'),
            Esc,
        ]);
        assert_eq!(map.count, 0);

        assert_eq!(String::from(map), "");
    }

    #[test]
    /// make sure large counts don't overflow
    fn vi_count_overflow() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);

        // make sure large counts don't overflow our u32
        simulate_keys!(map, [
            Esc,
            Char('9'), Char('9'), Char('9'), Char('9'), Char('9'),
            Char('9'), Char('9'), Char('9'), Char('9'), Char('9'),
            Char('9'), Char('9'), Char('9'), Char('9'), Char('9'),
            Char('9'), Char('9'), Char('9'), Char('9'), Char('9'),
            Char('9'), Char('9'), Char('9'), Char('9'), Char('9'),
            Char('9'), Char('9'), Char('9'), Char('9'), Char('9'),
            Char('9'), Char('9'), Char('9'), Char('9'), Char('9'),
            Char('9'), Char('9'), Char('9'), Char('9'), Char('9'),
            Char('9'), Char('9'), Char('9'), Char('9'), Char('9'),
            Char('9'), Char('9'), Char('9'), Char('9'), Char('9'),
            Char('9'), Char('9'), Char('9'), Char('9'), Char('9'),
            Char('9'), Char('9'), Char('9'), Char('9'), Char('9'),
            Char('9'), Char('9'), Char('9'), Char('9'), Char('9'),
        ]);
        assert_eq!(String::from(map), "");
    }

    #[test]
    /// make sure large counts ending in zero don't overflow
    fn vi_count_overflow_zero() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);

        // make sure large counts don't overflow our u32
        simulate_keys!(map, [
            Esc,
            Char('1'), Char('0'), Char('0'), Char('0'), Char('0'),
            Char('0'), Char('0'), Char('0'), Char('0'), Char('0'),
            Char('0'), Char('0'), Char('0'), Char('0'), Char('0'),
            Char('0'), Char('0'), Char('0'), Char('0'), Char('0'),
            Char('0'), Char('0'), Char('0'), Char('0'), Char('0'),
            Char('0'), Char('0'), Char('0'), Char('0'), Char('0'),
            Char('0'), Char('0'), Char('0'), Char('0'), Char('0'),
            Char('0'), Char('0'), Char('0'), Char('0'), Char('0'),
            Char('0'), Char('0'), Char('0'), Char('0'), Char('0'),
            Char('0'), Char('0'), Char('0'), Char('0'), Char('0'),
            Char('0'), Char('0'), Char('0'), Char('0'), Char('0'),
            Char('0'), Char('0'), Char('0'), Char('0'), Char('0'),
        ]);
        assert_eq!(String::from(map), "");
    }

    #[test]
    /// Esc should cancel the count
    fn vi_count_cancel() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);

        simulate_keys!(map, [
            Esc,
            Char('1'),
            Char('0'),
            Esc,
        ]);
        assert_eq!(map.count, 0);
        assert_eq!(String::from(map), "");
    }

    #[test]
    /// test insert with a count
    fn vi_count_simple() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);

        simulate_keys!(map, [
            Esc,
            Char('3'),
            Char('i'),
            Char('t'),
            Char('h'),
            Char('i'),
            Char('s'),
            Esc,
        ]);
        assert_eq!(String::from(map), "thisthisthis");
    }

    #[test]
    /// test dot command
    fn vi_dot_command() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);

        simulate_keys!(map, [
            Char('i'),
            Char('f'),
            Esc,
            Char('.'),
            Char('.'),
        ]);
        assert_eq!(String::from(map), "iiifff");
    }

    #[test]
    /// test dot command with repeat
    fn vi_dot_command_repeat() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);

        simulate_keys!(map, [
            Char('i'),
            Char('f'),
            Esc,
            Char('3'),
            Char('.'),
        ]);
        assert_eq!(String::from(map), "iifififf");
    }

    #[test]
    /// test dot command with repeat
    fn vi_dot_command_repeat_multiple() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);

        simulate_keys!(map, [
            Char('i'),
            Char('f'),
            Esc,
            Char('3'),
            Char('.'),
            Char('.'),
        ]);
        assert_eq!(String::from(map), "iififiifififff");
    }

    #[test]
    /// test dot command with append
    fn vi_dot_command_append() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);

        simulate_keys!(map, [
            Esc,
            Char('a'),
            Char('i'),
            Char('f'),
            Esc,
            Char('.'),
            Char('.'),
        ]);
        assert_eq!(String::from(map), "ififif");
    }

    #[test]
    /// test dot command with append and repeat
    fn vi_dot_command_append_repeat() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);

        simulate_keys!(map, [
            Esc,
            Char('a'),
            Char('i'),
            Char('f'),
            Esc,
            Char('3'),
            Char('.'),
        ]);
        assert_eq!(String::from(map), "ifififif");
    }

    #[test]
    /// test dot command with movement
    fn vi_dot_command_movement() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);

        simulate_keys!(map, [
            Esc,
            Char('a'),
            Char('d'),
            Char('t'),
            Char(' '),
            Left,
            Left,
            Char('a'),
            Esc,
            Right,
            Right,
            Char('.'),
        ]);
        assert_eq!(String::from(map), "data ");
    }

    #[test]
    /// test move_count function
    fn move_count() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);

        assert_eq!(map.move_count(), 1);
        map.count = 1;
        assert_eq!(map.move_count(), 1);
        map.count = 99;
        assert_eq!(map.move_count(), 99);
    }

    #[test]
    /// make sure the count is reset if movement occurs
    fn vi_count_movement_reset() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);

        simulate_keys!(map, [
            Esc,
            Char('3'),
            Char('i'),
            Char('t'),
            Char('h'),
            Char('i'),
            Char('s'),
            Left,
            Esc,
        ]);
        assert_eq!(String::from(map), "this");
    }

    #[test]
    /// test movement with counts
    fn movement_with_count() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("right").unwrap();
        assert_eq!(map.ed.cursor(), 5);

        simulate_keys!(map, [
            Esc,
            Char('3'),
            Left,
        ]);

        assert_eq!(map.ed.cursor(), 1);
    }

    #[test]
    /// test movement with counts, then insert (count should be reset before insert)
    fn movement_with_count_then_insert() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("right").unwrap();
        assert_eq!(map.ed.cursor(), 5);

        simulate_keys!(map, [
            Esc,
            Char('3'),
            Left,
            Char('i'),
            Char(' '),
            Esc,
        ]);
        assert_eq!(String::from(map), "r ight");
    }

    #[test]
    /// make sure we only attempt to repeat for as many chars are in the buffer
    fn count_at_buffer_edge() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("replace").unwrap();
        assert_eq!(map.ed.cursor(), 7);

        simulate_keys!(map, [
            Esc,
            Char('3'),
            Char('r'),
            Char('x'),
        ]);
        // the cursor should not have moved and no change should have occured
        assert_eq!(map.ed.cursor(), 6);
        assert_eq!(String::from(map), "replace");
    }

    #[test]
    /// test basic replace
    fn basic_replace() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("replace").unwrap();
        assert_eq!(map.ed.cursor(), 7);

        simulate_keys!(map, [
            Esc,
            Char('r'),
            Char('x'),
        ]);
        assert_eq!(map.ed.cursor(), 6);
        assert_eq!(String::from(map), "replacx");
    }

    #[test]
    fn replace_with_count() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("replace").unwrap();
        assert_eq!(map.ed.cursor(), 7);

        simulate_keys!(map, [
            Esc,
            Char('0'),
            Char('3'),
            Char('r'),
            Char(' '),
        ]);
        // cursor should be on the last replaced char
        assert_eq!(map.ed.cursor(), 2);
        assert_eq!(String::from(map), "   lace");
    }

    #[test]
    /// make sure replace won't work if there aren't enough chars
    fn replace_with_count_eol() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("replace").unwrap();
        assert_eq!(map.ed.cursor(), 7);

        simulate_keys!(map, [
            Esc,
            Char('3'),
            Char('r'),
            Char('x'),
        ]);
        // the cursor should not have moved and no change should have occured
        assert_eq!(map.ed.cursor(), 6);
        assert_eq!(String::from(map), "replace");
    }

    #[test]
    /// make sure normal mode is enabled after replace
    fn replace_then_normal() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("replace").unwrap();
        assert_eq!(map.ed.cursor(), 7);

        simulate_keys!(map, [
            Esc,
            Char('r'),
            Char('x'),
            Char('0'),
        ]);
        assert_eq!(map.ed.cursor(), 0);
        assert_eq!(String::from(map), "replacx");
    }

    #[test]
    /// test replace with dot
    fn dot_replace() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("replace").unwrap();
        assert_eq!(map.ed.cursor(), 7);

        simulate_keys!(map, [
            Esc,
            Char('0'),
            Char('r'),
            Char('x'),
            Char('.'),
            Char('.'),
            Char('7'),
            Char('.'),
        ]);
        assert_eq!(String::from(map), "xxxxxxx");
    }

    #[test]
    /// test replace with dot
    fn dot_replace_count() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("replace").unwrap();
        assert_eq!(map.ed.cursor(), 7);

        simulate_keys!(map, [
            Esc,
            Char('0'),
            Char('2'),
            Char('r'),
            Char('x'),
            Char('.'),
            Char('.'),
            Char('.'),
            Char('.'),
            Char('.'),
        ]);
        assert_eq!(String::from(map), "xxxxxxx");
    }

    #[test]
    /// test replace with dot at eol
    fn dot_replace_eol() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("test").unwrap();

        simulate_keys!(map, [
            Esc,
            Char('0'),
            Char('3'),
            Char('r'),
            Char('x'),
            Char('.'),
            Char('.'),
        ]);
        assert_eq!(String::from(map), "xxxt");
    }

    #[test]
    /// test replace with dot at eol multiple times
    fn dot_replace_eol_multiple() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("this is a test").unwrap();

        simulate_keys!(map, [
            Esc,
            Char('0'),
            Char('3'),
            Char('r'),
            Char('x'),
            Char('$'),
            Char('.'),
            Char('4'),
            Char('h'),
            Char('.'),
        ]);
        assert_eq!(String::from(map), "xxxs is axxxst");
    }

    #[test]
    /// verify our move count
    fn move_count_right() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("replace").unwrap();
        assert_eq!(map.ed.cursor(), 7);
        assert_eq!(map.move_count_right(), 0);
        map.count = 10;
        assert_eq!(map.move_count_right(), 0);

        map.count = 0;
        simulate_keys!(map, [
            Esc,
            Left,
        ]);
        assert_eq!(map.move_count_right(), 1);
    }

    #[test]
    /// verify our move count
    fn move_count_left() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("replace").unwrap();
        assert_eq!(map.ed.cursor(), 7);
        assert_eq!(map.move_count_left(), 1);
        map.count = 10;
        assert_eq!(map.move_count_left(), 7);

        map.count = 0;
        simulate_keys!(map, [
            Esc,
            Char('0'),
        ]);
        assert_eq!(map.move_count_left(), 0);
    }

    #[test]
    /// test delete with dot
    fn dot_x_delete() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("replace").unwrap();
        assert_eq!(map.ed.cursor(), 7);

        simulate_keys!(map, [
            Esc,
            Char('0'),
            Char('2'),
            Char('x'),
            Char('.'),
        ]);
        assert_eq!(String::from(map), "ace");
    }

    #[test]
    /// test deleting a line
    fn delete_line() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("delete").unwrap();

        simulate_keys!(map, [
            Esc,
            Char('d'),
            Char('d'),
        ]);
        assert_eq!(map.ed.cursor(), 0);
        assert_eq!(String::from(map), "");
    }

    #[test]
    /// test for normal mode after deleting a line
    fn delete_line_normal() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("delete").unwrap();

        simulate_keys!(map, [
            Esc,
            Char('d'),
            Char('d'),
            Char('i'),
            Char('n'),
            Char('e'),
            Char('w'),
            Esc,
        ]);
        assert_eq!(map.ed.cursor(), 2);
        assert_eq!(String::from(map), "new");
    }

    #[test]
    /// test aborting a delete (and change)
    fn delete_abort() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("don't delete").unwrap();

        simulate_keys!(map, [
            Esc,
            Char('d'),
            Esc,
            Char('d'),
            Char('c'),
            Char('c'),
            Char('d'),
        ]);
        assert_eq!(map.ed.cursor(), 11);
        assert_eq!(String::from(map), "don't delete");
    }

    #[test]
    /// test deleting a single char to the left
    fn delete_char_left() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("delete").unwrap();

        simulate_keys!(map, [
            Esc,
            Char('d'),
            Char('h'),
        ]);
        assert_eq!(map.ed.cursor(), 4);
        assert_eq!(String::from(map), "delee");
    }

    #[test]
    /// test deleting multiple chars to the left
    fn delete_chars_left() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("delete").unwrap();

        simulate_keys!(map, [
            Esc,
            Char('3'),
            Char('d'),
            Char('h'),
        ]);
        assert_eq!(map.ed.cursor(), 2);
        assert_eq!(String::from(map), "dee");
    }

    #[test]
    /// test deleting a single char to the right
    fn delete_char_right() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("delete").unwrap();

        simulate_keys!(map, [
            Esc,
            Char('0'),
            Char('d'),
            Char('l'),
        ]);
        assert_eq!(map.ed.cursor(), 0);
        assert_eq!(String::from(map), "elete");
    }

    #[test]
    /// test deleting multiple chars to the right
    fn delete_chars_right() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("delete").unwrap();

        simulate_keys!(map, [
            Esc,
            Char('0'),
            Char('3'),
            Char('d'),
            Char('l'),
        ]);
        assert_eq!(map.ed.cursor(), 0);
        assert_eq!(String::from(map), "ete");
    }

    #[test]
    /// test repeat with delete
    fn delete_and_repeat() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("delete").unwrap();

        simulate_keys!(map, [
            Esc,
            Char('0'),
            Char('d'),
            Char('l'),
            Char('.'),
        ]);
        assert_eq!(map.ed.cursor(), 0);
        assert_eq!(String::from(map), "lete");
    }

    #[test]
    /// test delete until end of line
    fn delete_until_end() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("delete").unwrap();

        simulate_keys!(map, [
            Esc,
            Char('0'),
            Char('d'),
            Char('$'),
        ]);
        assert_eq!(map.ed.cursor(), 0);
        assert_eq!(String::from(map), "");
    }

    #[test]
    /// test delete until end of line
    fn delete_until_end_shift_d() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("delete").unwrap();

        simulate_keys!(map, [
            Esc,
            Char('0'),
            Char('D'),
        ]);
        assert_eq!(map.ed.cursor(), 0);
        assert_eq!(String::from(map), "");
    }

    #[test]
    /// test delete until start of line
    fn delete_until_start() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("delete").unwrap();

        simulate_keys!(map, [
            Esc,
            Char('$'),
            Char('d'),
            Char('0'),
        ]);
        assert_eq!(map.ed.cursor(), 0);
        assert_eq!(String::from(map), "e");
    }

    #[test]
    /// test a compound count with delete
    fn delete_with_count() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("delete").unwrap();

        simulate_keys!(map, [
            Esc,
            Char('0'),
            Char('2'),
            Char('d'),
            Char('2'),
            Char('l'),
        ]);
        assert_eq!(map.ed.cursor(), 0);
        assert_eq!(String::from(map), "te");
    }

    #[test]
    /// test a compound count with delete and repeat
    fn delete_with_count_and_repeat() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("delete delete").unwrap();

        simulate_keys!(map, [
            Esc,
            Char('0'),
            Char('2'),
            Char('d'),
            Char('2'),
            Char('l'),
            Char('.'),
        ]);
        assert_eq!(map.ed.cursor(), 0);
        assert_eq!(String::from(map), "elete");
    }

    #[test]
    /// test changing a line
    fn change_line() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("change").unwrap();

        simulate_keys!(map, [
            Esc,
            Char('c'),
            Char('c'),
            Char('d'),
            Char('o'),
            Char('n'),
            Char('e'),
        ]);
        assert_eq!(map.ed.cursor(), 4);
        assert_eq!(String::from(map), "done");
    }

    #[test]
    /// test deleting a single char to the left
    fn change_char_left() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("change").unwrap();

        simulate_keys!(map, [
            Esc,
            Char('c'),
            Char('h'),
            Char('e'),
            Esc,
        ]);
        assert_eq!(map.ed.cursor(), 4);
        assert_eq!(String::from(map), "chanee");
    }

    #[test]
    /// test deleting multiple chars to the left
    fn change_chars_left() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("change").unwrap();

        simulate_keys!(map, [
            Esc,
            Char('3'),
            Char('c'),
            Char('h'),
            Char('e'),
        ]);
        assert_eq!(map.ed.cursor(), 3);
        assert_eq!(String::from(map), "chee");
    }

    #[test]
    /// test deleting a single char to the right
    fn change_char_right() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("change").unwrap();

        simulate_keys!(map, [
            Esc,
            Char('0'),
            Char('c'),
            Char('l'),
            Char('s'),
        ]);
        assert_eq!(map.ed.cursor(), 1);
        assert_eq!(String::from(map), "shange");
    }

    #[test]
    /// test changing multiple chars to the right
    fn change_chars_right() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("change").unwrap();

        simulate_keys!(map, [
            Esc,
            Char('0'),
            Char('3'),
            Char('c'),
            Char('l'),
            Char('s'),
            Char('t'),
            Char('r'),
            Char('a'),
            Esc,
        ]);
        assert_eq!(map.ed.cursor(), 3);
        assert_eq!(String::from(map), "strange");
    }

    #[test]
    /// test repeat with change
    fn change_and_repeat() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("change").unwrap();

        simulate_keys!(map, [
            Esc,
            Char('0'),
            Char('c'),
            Char('l'),
            Char('s'),
            Esc,
            Char('l'),
            Char('.'),
            Char('l'),
            Char('.'),
        ]);
        assert_eq!(map.ed.cursor(), 2);
        assert_eq!(String::from(map), "sssnge");
    }

    #[test]
    /// test change until end of line
    fn change_until_end() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("change").unwrap();

        simulate_keys!(map, [
            Esc,
            Char('0'),
            Char('c'),
            Char('$'),
            Char('o'),
            Char('k'),
            Esc,
        ]);
        assert_eq!(map.ed.cursor(), 1);
        assert_eq!(String::from(map), "ok");
    }

    #[test]
    /// test change until end of line
    fn change_until_end_shift_c() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("change").unwrap();

        simulate_keys!(map, [
            Esc,
            Char('0'),
            Char('C'),
            Char('o'),
            Char('k'),
        ]);
        assert_eq!(map.ed.cursor(), 2);
        assert_eq!(String::from(map), "ok");
    }

    #[test]
    /// test change until end of line
    fn change_until_end_from_middle_shift_c() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("change").unwrap();

        simulate_keys!(map, [
            Esc,
            Char('0'),
            Char('2'),
            Char('l'),
            Char('C'),
            Char(' '),
            Char('o'),
            Char('k'),
            Esc,
        ]);
        assert_eq!(String::from(map), "ch ok");
    }

    #[test]
    /// test change until start of line
    fn change_until_start() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("change").unwrap();

        simulate_keys!(map, [
            Esc,
            Char('$'),
            Char('c'),
            Char('0'),
            Char('s'),
            Char('t'),
            Char('r'),
            Char('a'),
            Char('n'),
            Char('g'),
        ]);
        assert_eq!(map.ed.cursor(), 6);
        assert_eq!(String::from(map), "strange");
    }

    #[test]
    /// test a compound count with change
    fn change_with_count() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("change").unwrap();

        simulate_keys!(map, [
            Esc,
            Char('0'),
            Char('2'),
            Char('c'),
            Char('2'),
            Char('l'),
            Char('s'),
            Char('t'),
            Char('r'),
            Char('a'),
            Char('n'),
            Esc,
        ]);
        assert_eq!(map.ed.cursor(), 4);
        assert_eq!(String::from(map), "strange");
    }

    #[test]
    /// test a compound count with change and repeat
    fn change_with_count_and_repeat() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("change change").unwrap();

        simulate_keys!(map, [
            Esc,
            Char('0'),
            Char('2'),
            Char('c'),
            Char('2'),
            Char('l'),
            Char('o'),
            Esc,
            Char('.'),
        ]);
        assert_eq!(map.ed.cursor(), 0);
        assert_eq!(String::from(map), "ochange");
    }

    #[test]
    /// undo with counts
    fn test_undo_with_counts() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("abcdefg").unwrap();

        simulate_keys!(map, [
            Esc,
            Char('x'),
            Char('x'),
            Char('x'),
            Char('3'),
            Char('u'),
        ]);
        assert_eq!(String::from(map), "abcdefg");
    }

    #[test]
    /// redo with counts
    fn test_redo_with_counts() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("abcdefg").unwrap();

        simulate_keys!(map, [
            Esc,
            Char('x'),
            Char('x'),
            Char('x'),
            Char('u'),
            Char('u'),
            Char('u'),
            Char('2'),
            Ctrl('r'),
        ]);
        assert_eq!(String::from(map), "abcde");
    }

    #[test]
    /// test undo in groups
    fn undo_insert() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);

        simulate_keys!(map, [
            Char('i'),
            Char('n'),
            Char('s'),
            Char('e'),
            Char('r'),
            Char('t'),
            Esc,
            Char('u'),
        ]);
        assert_eq!(String::from(map), "");
    }

    #[test]
    /// test undo in groups
    fn undo_insert2() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);

        simulate_keys!(map, [
            Esc,
            Char('i'),
            Char('i'),
            Char('n'),
            Char('s'),
            Char('e'),
            Char('r'),
            Char('t'),
            Esc,
            Char('u'),
        ]);
        assert_eq!(String::from(map), "");
    }

    #[test]
    /// test undo in groups
    fn undo_insert_with_history() {
        let mut context = Context::new();
        context.history.push(Buffer::from("")).unwrap();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);

        simulate_keys!(map, [
            Esc,
            Char('i'),
            Char('i'),
            Char('n'),
            Char('s'),
            Char('e'),
            Char('r'),
            Char('t'),
            Up,
            Char('h'),
            Char('i'),
            Char('s'),
            Char('t'),
            Char('o'),
            Char('r'),
            Char('y'),
            Down,
            Char(' '),
            Char('t'),
            Char('e'),
            Char('x'),
            Char('t'),
            Esc,
            Char('u'),
        ]);
        assert_eq!(String::from(map), "insert");
    }

    #[test]
    /// test undo in groups
    fn undo_insert_with_history2() {
        let mut context = Context::new();
        context.history.push(Buffer::from("")).unwrap();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);

        simulate_keys!(map, [
            Esc,
            Char('i'),
            Char('i'),
            Char('n'),
            Char('s'),
            Char('e'),
            Char('r'),
            Char('t'),
            Up,
            Esc,
            Down,
            Char('u'),
        ]);
        assert_eq!(String::from(map), "");
    }

    #[test]
    /// test undo in groups
    fn undo_insert_with_movement_reset() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);

        simulate_keys!(map, [
            Esc,
            Char('i'),
            Char('i'),
            Char('n'),
            Char('s'),
            Char('e'),
            Char('r'),
            Char('t'),
            // movement reset will get triggered here
            Left,
            Right,
            Char(' '),
            Char('t'),
            Char('e'),
            Char('x'),
            Char('t'),
            Esc,
            Char('u'),
        ]);
        assert_eq!(String::from(map), "insert");
    }

    #[test]
    /// test undo in groups
    fn undo_3x() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("rm some words").unwrap();

        simulate_keys!(map, [
            Esc,
            Char('0'),
            Char('3'),
            Char('x'),
            Char('u'),
        ]);
        assert_eq!(String::from(map), "rm some words");
    }

    #[test]
    /// test undo in groups
    fn undo_insert_with_count() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);

        simulate_keys!(map, [
            Char('i'),
            Char('n'),
            Char('s'),
            Char('e'),
            Char('r'),
            Char('t'),
            Esc,
            Char('3'),
            Char('i'),
            Char('i'),
            Char('n'),
            Char('s'),
            Char('e'),
            Char('r'),
            Char('t'),
            Esc,
            Char('u'),
        ]);
        assert_eq!(String::from(map), "insert");
    }

    #[test]
    /// test undo in groups
    fn undo_insert_with_repeat() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);

        simulate_keys!(map, [
            Char('i'),
            Char('n'),
            Char('s'),
            Char('e'),
            Char('r'),
            Char('t'),
            Esc,
            Char('3'),
            Char('.'),
            Esc,
            Char('u'),
        ]);
        assert_eq!(String::from(map), "insert");
    }

    #[test]
    /// test undo in groups
    fn undo_s_with_count() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("replace some words").unwrap();

        simulate_keys!(map, [
            Esc,
            Char('0'),
            Char('8'),
            Char('s'),
            Char('o'),
            Char('k'),
            Esc,
            Char('u'),
        ]);
        assert_eq!(String::from(map), "replace some words");
    }

    #[test]
    /// test undo in groups
    fn undo_multiple_groups() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("replace some words").unwrap();

        simulate_keys!(map, [
            Esc,
            Char('A'),
            Char(' '),
            Char('h'),
            Char('e'),
            Char('r'),
            Char('e'),
            Esc,
            Char('0'),
            Char('8'),
            Char('s'),
            Char('o'),
            Char('k'),
            Esc,
            Char('2'),
            Char('u'),
        ]);
        assert_eq!(String::from(map), "replace some words");
    }

    #[test]
    /// test undo in groups
    fn undo_r_command_with_count() {
        let mut context = Context::new();
        let out = Vec::new();
        let ed = Editor::new(out, "prompt".to_owned(), &mut context).unwrap();
        let mut map = Vi::new(ed);
        map.ed.insert_str_after_cursor("replace some words").unwrap();

        simulate_keys!(map, [
            Esc,
            Char('0'),
            Char('8'),
            Char('r'),
            Char(' '),
            Char('u'),
        ]);
        assert_eq!(String::from(map), "replace some words");
    }
}

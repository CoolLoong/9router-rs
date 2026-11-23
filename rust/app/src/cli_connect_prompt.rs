use std::io::{self, Write};
use std::path::Path;

use crossterm::cursor::{MoveLeft, MoveToColumn, MoveUp};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::queue;
use crossterm::terminal::{self, Clear, ClearType};

use crate::cli::Tool;

const TOOL_PROMPT: &str = "Select CLI tools to configure (space to toggle, enter to confirm)";
const PASSWORD_PROMPT: &str = "9router dashboard password";

pub(super) fn tools(home: &Path, config_home: Option<&Path>) -> io::Result<Option<Vec<Tool>>> {
    let _raw = RawTerminal::start()?;
    let mut output = io::stdout().lock();
    let choices = Tool::ALL;
    let hints: Vec<_> = choices
        .iter()
        .map(|tool| {
            tool.files(home, config_home)[0]
                .to_string_lossy()
                .chars()
                .filter(|ch| !ch.is_control())
                .collect::<String>()
        })
        .collect();
    let mut selected = [false; Tool::ALL.len()];
    let mut index: usize = 0;
    let mut first = 0;
    let mut lines = 0;
    let mut empty = false;

    loop {
        clear(&mut output, lines)?;
        let (cols, rows) = terminal_size();
        let visible = (usize::from(rows).saturating_sub(3)).clamp(1, choices.len());
        first = first.clamp(
            (index + 1).saturating_sub(visible),
            index.min(choices.len() - visible),
        );
        write!(output, "{}\r\n", clipped(&format!("? {TOOL_PROMPT}"), cols))?;
        for i in first..first + visible {
            let cursor = if i == index { ">" } else { " " };
            let mark = if selected[i] { "x" } else { " " };
            let label = format!("{cursor} [{mark}] {}  {}", choices[i].name(), hints[i]);
            write!(output, "{}\r\n", clipped(&label, cols))?;
        }
        if empty {
            write!(output, "Select at least one tool")?;
        }
        lines = (visible + 1) as u16;
        output.flush()?;

        let key = match event::read()? {
            Event::Key(key) if key.kind != KeyEventKind::Release => key,
            Event::Resize(..) => continue,
            _ => continue,
        };
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                clear(&mut output, lines)?;
                output.flush()?;
                return Ok(None);
            }
            (KeyCode::Enter, _) => {
                let result: Vec<_> = choices
                    .into_iter()
                    .zip(selected)
                    .filter_map(|(tool, active)| active.then_some(tool))
                    .collect();
                if result.is_empty() {
                    empty = true;
                    continue;
                }
                clear(&mut output, lines)?;
                let answer = result
                    .iter()
                    .map(|tool| tool.name())
                    .collect::<Vec<_>>()
                    .join(", ");
                writeln!(
                    output,
                    "{}\r",
                    clipped(&format!("? {TOOL_PROMPT} · {answer}"), cols)
                )?;
                output.flush()?;
                return Ok(Some(result));
            }
            (KeyCode::Up, _) => index = (index + choices.len() - 1) % choices.len(),
            (KeyCode::Down, _) => index = (index + 1) % choices.len(),
            (KeyCode::Home, _) => index = 0,
            (KeyCode::End, _) => index = choices.len() - 1,
            (KeyCode::Char(' '), _) => selected[index] = !selected[index],
            (KeyCode::Char('a' | 'g'), KeyModifiers::NONE) => {
                let enabled = selected.iter().all(|active| *active);
                selected.fill(!enabled);
            }
            (KeyCode::Char('i'), KeyModifiers::NONE) => {
                selected.iter_mut().for_each(|active| *active = !*active)
            }
            _ => (),
        }
        if selected.iter().any(|active| *active) {
            empty = false;
        }
    }
}

pub(super) fn password() -> io::Result<Option<String>> {
    let _raw = RawTerminal::start()?;
    let mut output = io::stdout().lock();
    let mut text = Vec::new();
    let mut cursor: usize = 0;

    loop {
        clear(&mut output, 0)?;
        let (cols, _) = terminal_size();
        let label = format!("? {PASSWORD_PROMPT} · ");
        let displayed_label = clipped(&label, cols);
        let width = usize::from(cols).saturating_sub(displayed_label.chars().count() + 1);
        let mask_start = cursor.saturating_sub(width);
        let mask_end = (mask_start + width).min(text.len());
        let mask_len = mask_end.saturating_sub(mask_start);
        write!(output, "{displayed_label}{}", "*".repeat(mask_len))?;
        if let Ok(after_cursor) = u16::try_from(mask_end.saturating_sub(cursor))
            && after_cursor > 0
        {
            queue!(output, MoveLeft(after_cursor))?;
        }
        output.flush()?;

        let key = match event::read()? {
            Event::Key(key) if key.kind != KeyEventKind::Release => key,
            _ => continue,
        };
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                clear(&mut output, 0)?;
                output.flush()?;
                return Ok(None);
            }
            (KeyCode::Enter, _) => {
                write!(output, "\r\n")?;
                output.flush()?;
                return Ok(Some(text.into_iter().collect()));
            }
            (KeyCode::Backspace, _) if cursor > 0 => {
                cursor -= 1;
                text.remove(cursor);
            }
            (KeyCode::Delete, _) if cursor < text.len() => {
                text.remove(cursor);
            }
            (KeyCode::Left, _) => cursor = cursor.saturating_sub(1),
            (KeyCode::Right, _) => cursor = (cursor + 1).min(text.len()),
            (KeyCode::Home, _) | (KeyCode::Char('a'), KeyModifiers::CONTROL) => cursor = 0,
            (KeyCode::End, _) | (KeyCode::Char('e'), KeyModifiers::CONTROL) => cursor = text.len(),
            (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                text.drain(..cursor);
                cursor = 0;
            }
            (KeyCode::Char('k'), KeyModifiers::CONTROL) => text.truncate(cursor),
            (KeyCode::Char(ch), modifiers)
                if !ch.is_control()
                    && !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                text.insert(cursor, ch);
                cursor += 1;
            }
            _ => (),
        }
    }
}

struct RawTerminal;

impl RawTerminal {
    fn start() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

fn terminal_size() -> (u16, u16) {
    terminal::size()
        .ok()
        .filter(|(cols, rows)| *cols > 0 && *rows > 0)
        .unwrap_or((80, 24))
}

fn clear(output: &mut impl Write, lines: u16) -> io::Result<()> {
    if lines > 0 {
        queue!(output, MoveUp(lines))?;
    }
    queue!(output, MoveToColumn(0), Clear(ClearType::FromCursorDown))
}

fn clipped(text: &str, columns: u16) -> String {
    let mut remaining = columns.saturating_sub(1);
    text.chars()
        .take_while(|ch| {
            let width = if ch.is_ascii() { 1 } else { 2 };
            if remaining < width {
                return false;
            }
            remaining -= width;
            true
        })
        .collect()
}

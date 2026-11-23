use std::io::{self, Write};
use std::time::Duration;

use crossterm::cursor::{MoveTo, MoveToColumn};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::queue;
use crossterm::style::{Attribute, Color, ResetColor, SetAttribute, SetForegroundColor};
use crossterm::terminal::{self, Clear, ClearType};

pub(super) async fn menu(
    title: &str,
    header: &str,
    labels: &[String],
) -> io::Result<Option<usize>> {
    let _terminal = RawTerminal::start()?;
    let mut selected = 0;
    loop {
        render_menu(title, header, labels, selected)?;
        match key().await?.code {
            KeyCode::Esc => return Ok(None),
            KeyCode::Enter => return Ok(Some(selected)),
            KeyCode::Up => selected = (selected + labels.len() - 1) % labels.len(),
            KeyCode::Down => selected = (selected + 1) % labels.len(),
            KeyCode::Home => selected = 0,
            KeyCode::End => selected = labels.len() - 1,
            _ => (),
        }
    }
}

fn render_menu(title: &str, header: &str, labels: &[String], selected: usize) -> io::Result<()> {
    let mut out = io::stdout().lock();
    let (width, height) = terminal::size()
        .ok()
        .filter(|(width, height)| *width > 1 && *height > 1)
        .unwrap_or((80, 24));
    #[cfg(windows)]
    let color = Color::Yellow;
    #[cfg(not(windows))]
    let color = Color::Rgb {
        r: 217,
        g: 119,
        b: 87,
    };
    queue!(
        out,
        Clear(ClearType::All),
        MoveTo(0, 0),
        SetForegroundColor(color)
    )?;
    write!(
        out,
        "{}\r\n  {}\r\n{}\r\n",
        "=".repeat(usize::from(width.min(40))),
        clip(title, width),
        "=".repeat(usize::from(width.min(40)))
    )?;
    queue!(out, ResetColor)?;
    let header_lines = header.lines().take(usize::from(height).saturating_sub(8));
    let mut rows_used = 4;
    for line in header_lines {
        write!(out, "{}\r\n", clip(line, width))?;
        rows_used += 1;
    }
    let visible = usize::from(height.saturating_sub(rows_used)).max(1);
    let first = selected.saturating_sub(visible - 1);
    for (index, label) in labels.iter().enumerate().skip(first).take(visible) {
        let line = format!("{} {}", if index == selected { "★" } else { "☆" }, label);
        if index == selected {
            queue!(
                out,
                SetAttribute(Attribute::Reverse),
                SetAttribute(Attribute::Bold)
            )?;
            write!(out, "{}", clip(&line, width))?;
            queue!(out, SetAttribute(Attribute::Reset))?;
            write!(out, "\r\n")?;
        } else {
            write!(out, "{}\r\n", clip(&line, width))?;
        }
    }
    out.flush()
}

pub(super) async fn prompt(message: &str, hidden: bool) -> io::Result<Option<String>> {
    let _terminal = RawTerminal::start()?;
    let mut text = String::new();
    loop {
        render_prompt(message, &text, hidden)?;
        match key().await?.code {
            KeyCode::Esc => {
                finish_line()?;
                return Ok(None);
            }
            KeyCode::Enter => {
                finish_line()?;
                return Ok(Some(text.trim().to_owned()));
            }
            KeyCode::Backspace => {
                text.pop();
            }
            KeyCode::Char(ch) if !ch.is_control() => {
                text.push(ch);
            }
            _ => (),
        }
    }
}

fn render_prompt(message: &str, text: &str, hidden: bool) -> io::Result<()> {
    let mut out = io::stdout().lock();
    queue!(out, MoveToColumn(0), Clear(ClearType::CurrentLine))?;
    if hidden {
        write!(out, "{message}{}", "*".repeat(text.chars().count()))?;
    } else {
        write!(out, "{message}{text}")?;
    }
    out.flush()
}

fn finish_line() -> io::Result<()> {
    let mut out = io::stdout().lock();
    write!(out, "\r\n")?;
    out.flush()
}

pub(super) async fn confirm(message: &str) -> io::Result<bool> {
    let message = format!("{message} (y/n): ");
    loop {
        match prompt(&message, false)
            .await?
            .as_deref()
            .map(str::to_lowercase)
            .as_deref()
        {
            Some("y" | "yes") => return Ok(true),
            None | Some("n" | "no") => return Ok(false),
            _ => println!("Please answer 'y' or 'n'"),
        }
    }
}

pub(super) async fn pause() -> io::Result<()> {
    prompt("Press Enter to continue...", false)
        .await
        .map(|_| ())
}

struct RawTerminal {
    #[cfg(windows)]
    _mode: ConsoleMode,
}

impl RawTerminal {
    fn start() -> io::Result<Self> {
        #[cfg(windows)]
        let _mode = ConsoleMode::capture(windows_sys::Win32::System::Console::STD_INPUT_HANDLE)?;
        terminal::enable_raw_mode()?;
        Ok(Self {
            #[cfg(windows)]
            _mode,
        })
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        #[cfg(not(windows))]
        let _ = terminal::disable_raw_mode();
    }
}

#[cfg(windows)]
pub(super) struct ConsoleMode {
    handle: usize,
    mode: u32,
}

#[cfg(windows)]
impl ConsoleMode {
    pub(super) fn capture(kind: u32) -> io::Result<Self> {
        use windows_sys::Win32::System::Console::{GetConsoleMode, GetStdHandle};
        let handle = unsafe { GetStdHandle(kind) };
        let mut mode = 0;
        if unsafe { GetConsoleMode(handle, &mut mode) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            handle: handle as usize,
            mode,
        })
    }
}

#[cfg(windows)]
impl Drop for ConsoleMode {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::System::Console::SetConsoleMode(self.handle as _, self.mode) };
    }
}

async fn key() -> io::Result<KeyEvent> {
    loop {
        if event::poll(Duration::ZERO)?
            && let Event::Key(event) = event::read()?
            && event.kind != KeyEventKind::Release
        {
            if event.modifiers.contains(KeyModifiers::CONTROL) {
                if event.code == KeyCode::Char('c') {
                    return Err(io::Error::from(io::ErrorKind::Interrupted));
                }
            } else {
                return Ok(event);
            }
        }
        tokio::time::sleep(Duration::from_millis(16)).await;
    }
}

fn clip(text: &str, width: u16) -> String {
    let mut remaining = width.saturating_sub(1);
    text.chars()
        .filter(|ch| !ch.is_control())
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

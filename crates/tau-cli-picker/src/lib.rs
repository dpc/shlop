use std::{fmt, io};

use tau_term_screen::screen::Screen;
use tau_term_screen::style::StyledText;

#[derive(Clone, Debug)]
pub struct PickerItem {
    pub label: String,
    pub enabled: bool,
}

impl PickerItem {
    pub fn enabled(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            enabled: true,
        }
    }

    pub fn disabled(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            enabled: false,
        }
    }
}

#[derive(Debug)]
pub enum PickerError {
    Io(io::Error),
    Empty,
    NoEnabledItems,
}

impl fmt::Display for PickerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(source) => write!(f, "I/O error: {source}"),
            Self::Empty => f.write_str("picker has no items"),
            Self::NoEnabledItems => f.write_str("picker has no enabled items"),
        }
    }
}

impl std::error::Error for PickerError {}

impl From<io::Error> for PickerError {
    fn from(source: io::Error) -> Self {
        Self::Io(source)
    }
}

pub fn pick(prompt: &str, items: &[PickerItem]) -> Result<usize, PickerError> {
    let _raw = RawModeGuard::enable()?;
    pick_with_key_reader(prompt, items, io::stderr(), read_terminal_key)
}

pub fn pick_with_io(
    prompt: &str,
    items: &[PickerItem],
    writer: impl io::Write,
    mut reader: impl io::Read,
) -> Result<usize, PickerError> {
    pick_with_key_reader(prompt, items, writer, || read_key(&mut reader))
}

fn pick_with_key_reader(
    prompt: &str,
    items: &[PickerItem],
    mut writer: impl io::Write,
    mut read_key: impl FnMut() -> io::Result<PickerKey>,
) -> Result<usize, PickerError> {
    if items.is_empty() {
        return Err(PickerError::Empty);
    }
    let mut selected = items
        .iter()
        .position(|item| item.enabled)
        .ok_or(PickerError::NoEnabledItems)?;
    let mut screen = Screen::new(terminal_width());

    render(&mut screen, &mut writer, prompt, items, selected)?;
    loop {
        match read_key()? {
            PickerKey::Down => selected = adjacent_enabled_item(items, selected, true),
            PickerKey::Up => selected = adjacent_enabled_item(items, selected, false),
            PickerKey::Enter => {
                screen.update(&mut writer, &[], (0, 0))?;
                return Ok(selected);
            }
            PickerKey::Cancelled => {
                screen.update(&mut writer, &[], (0, 0))?;
                return Err(io::Error::new(io::ErrorKind::Interrupted, "picker cancelled").into());
            }
            PickerKey::Ignored => {}
        }
        render(&mut screen, &mut writer, prompt, items, selected)?;
    }
}

fn render(
    screen: &mut Screen,
    writer: &mut impl io::Write,
    prompt: &str,
    items: &[PickerItem],
    selected: usize,
) -> io::Result<()> {
    let mut lines = Vec::with_capacity(items.len() + 1);
    lines.push(StyledText::from(format!("? {prompt}")).to_cells());
    for (idx, item) in items.iter().enumerate() {
        let marker = if !item.enabled {
            'X'
        } else if idx == selected {
            '>'
        } else {
            ' '
        };
        lines.push(StyledText::from(format!("{marker} {}", item.label)).to_cells());
    }
    screen.update(writer, &lines, (selected + 1, 0))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PickerKey {
    Up,
    Down,
    Enter,
    Cancelled,
    Ignored,
}

struct RawModeGuard;

impl RawModeGuard {
    fn enable() -> io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

fn read_terminal_key() -> io::Result<PickerKey> {
    loop {
        let event = crossterm::event::read()?;
        let crossterm::event::Event::Key(key) = event else {
            continue;
        };
        return Ok(match key.code {
            crossterm::event::KeyCode::Up => PickerKey::Up,
            crossterm::event::KeyCode::Down | crossterm::event::KeyCode::Tab => PickerKey::Down,
            crossterm::event::KeyCode::BackTab => PickerKey::Up,
            crossterm::event::KeyCode::Char('j') => PickerKey::Down,
            crossterm::event::KeyCode::Char('k') => PickerKey::Up,
            crossterm::event::KeyCode::Char('c')
                if key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::CONTROL) =>
            {
                PickerKey::Cancelled
            }
            crossterm::event::KeyCode::Char(' ') | crossterm::event::KeyCode::Enter => {
                PickerKey::Enter
            }
            _ => PickerKey::Ignored,
        });
    }
}

fn read_key(reader: &mut impl io::Read) -> io::Result<PickerKey> {
    let mut b = [0_u8; 1];
    reader.read_exact(&mut b)?;
    match b[0] {
        0x03 => Ok(PickerKey::Cancelled),
        b'\n' | b'\r' | b' ' => Ok(PickerKey::Enter),
        b'j' | b'\t' => Ok(PickerKey::Down),
        b'k' => Ok(PickerKey::Up),
        0x1b => read_escape_key(reader),
        _ => Ok(PickerKey::Ignored),
    }
}

fn read_escape_key(reader: &mut impl io::Read) -> io::Result<PickerKey> {
    let mut b = [0_u8; 2];
    if reader.read_exact(&mut b).is_err() {
        return Ok(PickerKey::Ignored);
    }
    match b {
        [b'[', b'A'] => Ok(PickerKey::Up),
        [b'[', b'B'] => Ok(PickerKey::Down),
        _ => Ok(PickerKey::Ignored),
    }
}

fn adjacent_enabled_item(items: &[PickerItem], selected: usize, forward: bool) -> usize {
    for offset in 1..items.len() {
        let idx = if forward {
            (selected + offset) % items.len()
        } else {
            (selected + items.len() - offset) % items.len()
        };
        if items[idx].enabled {
            return idx;
        }
    }
    selected
}

fn terminal_width() -> usize {
    crossterm_size().map_or(80, |(width, _)| width.into())
}

fn crossterm_size() -> io::Result<(u16, u16)> {
    crossterm::terminal::size()
}

//! The terminal interface: a bar waveform with transport controls under it.

use std::io;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Alignment, Position, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};

use crate::decode::{self, Track};
use crate::player::{Link, Player, Status};
use crate::proto::RATE;

// Colours come from the 256-colour palette so they look the same in every
// terminal, including ones without truecolor. `Reset` is the terminal's own
// foreground, which stays readable on light and dark backgrounds alike.
const PLAYED: Color = Color::Indexed(208);
const HEAD: Color = Color::Reset;
const AHEAD: Color = Color::Indexed(240);
const DIM: Color = Color::Indexed(245);
const FAINT: Color = Color::Indexed(240);
const GOOD: Color = Color::Indexed(114);
const WARN: Color = Color::Indexed(179);
const BAD: Color = Color::Indexed(203);

const MAX_WIDTH: u16 = 160;
const MAX_WAVE_ROWS: u16 = 12;
const BUTTON_WIDTH: u16 = 6;

/// One-eighth steps of a cell, filled from the bottom.
const EIGHTHS: [&str; 9] = [" ", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];

#[derive(Clone, Copy, PartialEq)]
enum Act {
    Prev,
    Back,
    Toggle,
    Forward,
    Next,
}

enum View {
    Loading,
    Ready(Track),
    Failed(String),
}

struct App {
    files: Vec<PathBuf>,
    idx: usize,
    /// Which way we were moving through the files, to skip unplayable ones.
    direction: isize,
    view: View,
    player: Player,
    /// Percent.
    volume: u8,
    decoded: (Sender<(u64, Result<Track>)>, Receiver<(u64, Result<Track>)>),
    load_id: u64,
    bars: Vec<f32>,
    // Where things were last drawn, for the mouse.
    wave: Rect,
    buttons: Vec<(Rect, Act)>,
    scrubbing: bool,
    quit: bool,
}

pub fn run(files: Vec<PathBuf>, player: Player) -> Result<()> {
    let mut terminal = ratatui::init();
    // ratatui's panic hook restores the screen; make sure the mouse goes back too.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(io::stdout(), DisableMouseCapture);
        hook(info);
    }));
    let _ = execute!(io::stdout(), EnableMouseCapture);

    let mut app = App {
        files,
        idx: 0,
        direction: 1,
        view: View::Loading,
        player,
        volume: 100,
        decoded: mpsc::channel(),
        load_id: 0,
        bars: Vec::new(),
        wave: Rect::default(),
        buttons: Vec::new(),
        scrubbing: false,
        quit: false,
    };
    let result = app.run(&mut terminal);

    let _ = execute!(io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

impl App {
    fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        self.open(0);
        while !self.quit {
            self.tick();
            terminal.draw(|frame| self.draw(frame))?;
            if event::poll(Duration::from_millis(50))? {
                loop {
                    match event::read()? {
                        Event::Key(key) => self.on_key(key),
                        Event::Mouse(mouse) => self.on_mouse(mouse),
                        _ => {}
                    }
                    if !event::poll(Duration::ZERO)? {
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    fn open(&mut self, idx: usize) {
        self.idx = idx;
        self.view = View::Loading;
        self.bars.clear();
        self.player.unload();
        self.load_id += 1;
        let (id, path, done) = (self.load_id, self.files[idx].clone(), self.decoded.0.clone());
        thread::spawn(move || {
            let _ = done.send((id, decode::decode(&path)));
        });
    }

    fn neighbour(&self, step: isize) -> Option<usize> {
        self.idx.checked_add_signed(step).filter(|&i| i < self.files.len())
    }

    fn step_to(&mut self, step: isize) -> bool {
        let Some(idx) = self.neighbour(step) else { return false };
        self.direction = step;
        self.open(idx);
        true
    }

    fn tick(&mut self) {
        while let Ok((id, result)) = self.decoded.1.try_recv() {
            if id != self.load_id {
                continue; // a file we have already moved on from
            }
            match result {
                Ok(track) => {
                    self.player.load(track.pcm.clone());
                    self.view = View::Ready(track);
                }
                // Skip over things that are not audio when working through a list.
                Err(e) => {
                    if !self.step_to(self.direction) {
                        self.view = View::Failed(format!("{e:#}"));
                    }
                }
            }
        }
        if matches!(self.view, View::Ready(_)) {
            let status = self.player.status();
            if status.ended && status.playing && !self.step_to(1) {
                self.player.set_playing(false);
            }
        }
    }

    fn frames(&self) -> u64 {
        match &self.view {
            View::Ready(track) => track.frames,
            _ => 0,
        }
    }

    fn act(&mut self, act: Act) {
        let total = self.frames();
        let status = self.player.status();
        // Short sounds get proportionally shorter skips.
        let skip = if total < 30 * RATE as u64 { (total / 10).max(1) } else { 5 * RATE as u64 };
        match act {
            Act::Toggle if total > 0 => {
                if status.ended {
                    self.player.seek(0);
                    self.player.set_playing(true);
                } else {
                    self.player.set_playing(!status.playing);
                }
            }
            Act::Back if total > 0 => self.player.seek(status.pos.saturating_sub(skip)),
            Act::Forward if total > 0 => self.player.seek(status.pos + skip),
            Act::Next => {
                self.step_to(1);
            }
            Act::Prev => {
                if !self.step_to(-1) {
                    self.player.seek(0);
                }
            }
            _ => {}
        }
    }

    fn set_volume(&mut self, percent: i16) {
        self.volume = percent.clamp(0, 100) as u8;
        // Squared, so the steps sound even.
        let gain = self.volume as f32 / 100.0;
        self.player.set_volume(gain * gain);
    }

    fn on_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        let volume = self.volume as i16;
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char(' ') | KeyCode::Enter => self.act(Act::Toggle),
            KeyCode::Left | KeyCode::Char('h') => self.act(Act::Back),
            KeyCode::Right | KeyCode::Char('l') => self.act(Act::Forward),
            KeyCode::Up | KeyCode::Char('k' | '+' | '=') => self.set_volume(volume + 5),
            KeyCode::Down | KeyCode::Char('j' | '-') => self.set_volume(volume - 5),
            KeyCode::Home | KeyCode::Char('0') => self.player.seek(0),
            KeyCode::Char('n') => self.act(Act::Next),
            KeyCode::Char('p') => self.act(Act::Prev),
            _ => {}
        }
    }

    fn on_mouse(&mut self, mouse: MouseEvent) {
        let at = Position::new(mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if self.wave.contains(at) {
                    self.scrubbing = true;
                    self.seek_to_column(mouse.column);
                } else if let Some(&(_, act)) = self.buttons.iter().find(|(rect, _)| rect.contains(at)) {
                    self.act(act);
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.scrubbing => self.seek_to_column(mouse.column),
            MouseEventKind::Up(_) => self.scrubbing = false,
            _ => {}
        }
    }

    fn seek_to_column(&mut self, column: u16) {
        if self.wave.width == 0 {
            return;
        }
        let column = column.clamp(self.wave.left(), self.wave.right() - 1) - self.wave.left();
        let fraction = (column as f64 + 0.5) / self.wave.width as f64;
        self.player.seek((fraction * self.frames() as f64) as u64);
    }

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        self.wave = Rect::default();
        self.buttons.clear();
        if area.width < 28 || area.height < 5 {
            frame.render_widget(Line::styled("window too small", DIM), area);
            return;
        }
        let status = self.player.status();

        // title / waveform / transport / footer, with blank rows between and
        // around them when there is height to spare.
        let gap = (area.height >= 12) as u16;
        let width = (area.width - 4).min(MAX_WIDTH);
        let x = area.x + (area.width - width) / 2;
        let chrome = 3 + 3 * gap;
        let wave_rows = (area.height - chrome - 2 * gap).min(MAX_WAVE_ROWS) & !1;
        let mut y = area.y + (area.height - chrome - wave_rows) / 2;
        let row = |y: u16, height: u16| Rect::new(x, y, width, height);

        self.draw_title(frame, row(y, 1));
        y += 1 + gap;
        self.draw_wave(frame, row(y, wave_rows), &status);
        y += wave_rows + gap;
        self.draw_transport(frame, row(y, 1), &status);
        y += 1 + gap;
        self.draw_footer(frame, row(y, 1), &status);
    }

    fn draw_title(&self, frame: &mut Frame, area: Rect) {
        let path = &self.files[self.idx];
        let name = path.file_name().unwrap_or(path.as_os_str()).to_string_lossy();

        let mut details = Vec::new();
        if self.files.len() > 1 {
            details.push(format!("{}/{}", self.idx + 1, self.files.len()));
        }
        if let View::Ready(track) = &self.view
            && !track.info.is_empty()
        {
            details.push(track.info.clone());
        }
        let details = Line::styled(details.join("   "), DIM).alignment(Alignment::Right);
        let mut name_width = area.width;
        if details.width() as u16 + 12 <= area.width {
            name_width -= details.width() as u16 + 2;
            frame.render_widget(details, area);
        }
        frame.render_widget(Line::from(name).bold(), Rect { width: name_width, ..area });
    }

    fn draw_wave(&mut self, frame: &mut Frame, area: Rect, status: &Status) {
        let track = match &self.view {
            View::Ready(track) => track,
            View::Loading => return centred(frame, area, "decoding…", DIM),
            View::Failed(why) => return centred(frame, area, why, BAD),
        };
        // One column of bar, one of space.
        let count = (area.width as usize).div_ceil(2);
        if self.bars.len() != count {
            self.bars = track.envelope.bars(count);
        }
        let head = if status.ended {
            count
        } else {
            (status.pos as u128 * count as u128 / track.frames.max(1) as u128) as usize
        };
        bars(frame.buffer_mut(), area, &self.bars, head);
        self.wave = area;
    }

    fn draw_transport(&mut self, frame: &mut Frame, area: Rect, status: &Status) {
        let total = self.frames();
        let pos = if total > 0 { status.pos } else { 0 };
        let time = Line::from(vec![
            Span::raw(clock(pos, total)),
            Span::styled(format!(" / {}", clock(total, total)), DIM),
        ]);
        frame.render_widget(time, area);

        if area.width >= 60 {
            let lit = (self.volume as usize + 5) / 10;
            let volume = Line::from(vec![
                Span::styled("vol ", DIM),
                Span::raw("━".repeat(lit)),
                Span::styled("━".repeat(10 - lit), FAINT),
            ]);
            frame.render_widget(volume.alignment(Alignment::Right), area);
        }

        let toggle = if status.playing && total > 0 { "▌▌" } else { "▶" };
        let mut buttons = vec![(Act::Back, "◀◀"), (Act::Toggle, toggle), (Act::Forward, "▶▶")];
        if self.files.len() > 1 {
            buttons.insert(0, (Act::Prev, "▕◀"));
            buttons.push((Act::Next, "▶▏"));
        }
        let span = BUTTON_WIDTH * buttons.len() as u16;
        let mut x = area.x + area.width.saturating_sub(span) / 2;
        for (act, label) in buttons {
            let rect = Rect::new(x, area.y, BUTTON_WIDTH, 1).intersection(area);
            let style = if act == Act::Toggle { Style::new().bold() } else { Style::new().fg(DIM) };
            frame.render_widget(Line::styled(label, style).alignment(Alignment::Center), rect);
            self.buttons.push((rect, act));
            x += BUTTON_WIDTH;
        }
    }

    fn draw_footer(&self, frame: &mut Frame, area: Rect, status: &Status) {
        let link = match &status.link {
            Link::Connected(name) => Line::from(vec![Span::styled("● ", GOOD), Span::styled(name.as_str(), DIM)]),
            Link::Waiting(addr) => Line::from(vec![
                Span::styled("○ ", WARN),
                Span::styled(format!("waiting for `sshound listen` on {addr}"), DIM),
            ]),
        };
        let room = (area.width as usize).saturating_sub(link.width() + 3);
        frame.render_widget(link, area);

        let files = if self.files.len() > 1 { " · n/p file" } else { "" };
        let keys = [
            format!("space play/pause · ←/→ seek · ↑/↓ volume{files} · q quit"),
            format!("space · ←/→ · ↑/↓{} · q", if files.is_empty() { "" } else { " · n/p" }),
        ];
        if let Some(keys) = keys.iter().find(|k| Line::raw(k.as_str()).width() <= room) {
            frame.render_widget(Line::styled(keys.as_str(), FAINT).alignment(Alignment::Right), area);
        }
    }
}

fn centred(frame: &mut Frame, area: Rect, text: &str, color: Color) {
    let rows = area.height.min(2);
    let area = Rect { y: area.y + (area.height - rows) / 2, height: rows, ..area };
    let text = Paragraph::new(text).style(color).alignment(Alignment::Center).wrap(Wrap { trim: true });
    frame.render_widget(text, area);
}

/// Draw `heights` (0..=1) as bars mirrored about the middle of `area`, with
/// eighth-of-a-cell resolution. Bars before `head` are played, the one at
/// `head` is the playhead.
fn bars(buf: &mut Buffer, area: Rect, heights: &[f32], head: usize) {
    let half = (area.height / 2) as usize;
    let levels = half * 8;
    for (i, &height) in heights.iter().enumerate() {
        let x = area.x + 2 * i as u16;
        if half == 0 || x >= area.right() {
            break;
        }
        let color = match i {
            _ if i < head => PLAYED,
            _ if i == head => HEAD,
            _ => AHEAD,
        };
        let style = Style::new().fg(color);
        // Even silence gets a sliver, so the track reads as a continuous line.
        let level = ((height * levels as f32).round() as usize).clamp(1, levels);
        for row in 0..half {
            // Above the middle: cells fill from the bottom up.
            let fill = level.saturating_sub((half - 1 - row) * 8).min(8);
            buf[(x, area.y + row as u16)].set_symbol(EIGHTHS[fill]).set_style(style);
            // Below it they must fill from the top down. There are no glyphs
            // for that, so draw the complement and swap the colours.
            let fill = level.saturating_sub(row * 8).min(8);
            let cell = &mut buf[(x, area.y + (half + row) as u16)];
            match fill {
                0 => cell.set_symbol(" "),
                8 => cell.set_symbol("█").set_style(style),
                n => cell.set_symbol(EIGHTHS[8 - n]).set_style(style.add_modifier(Modifier::REVERSED)),
            };
        }
    }
}

/// `m:ss`, growing hours for long files and tenths for very short ones.
fn clock(frames: u64, total: u64) -> String {
    let rate = RATE as u64;
    let secs = frames / rate;
    if total < 10 * rate {
        format!("{}:{:02}.{}", secs / 60, secs % 60, frames % rate * 10 / rate)
    } else if total < 3600 * rate {
        format!("{}:{:02}", secs / 60, secs % 60)
    } else {
        format!("{}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(buf: &Buffer) -> Vec<String> {
        let area = buf.area;
        (area.top()..area.bottom())
            .map(|y| (area.left()..area.right()).map(|x| buf[(x, y)].symbol()).collect())
            .collect()
    }

    #[test]
    fn bars_are_mirrored_about_the_middle() {
        let area = Rect::new(0, 0, 7, 4);
        let mut buf = Buffer::empty(area);
        // 16 levels per half: silent, 4/16, 12/16, full.
        bars(&mut buf, area, &[0.0, 0.25, 0.75, 1.0], 2);
        assert_eq!(rows(&buf), ["    ▄ █", "▁ ▄ █ █", "▇ ▄ █ █", "    ▄ █"]);

        // The lower half is drawn inverted, except where a cell is full or empty.
        let reversed = |x, y| buf[(x, y)].modifier.contains(Modifier::REVERSED);
        assert!(reversed(0, 2) && reversed(2, 2) && reversed(4, 3));
        assert!(!reversed(4, 2) && !reversed(6, 3) && !reversed(0, 3) && !reversed(2, 1));

        // Played, playhead, not yet played.
        assert_eq!(buf[(0, 1)].fg, PLAYED);
        assert_eq!(buf[(2, 1)].fg, PLAYED);
        assert_eq!(buf[(4, 1)].fg, HEAD);
        assert_eq!(buf[(6, 1)].fg, AHEAD);
    }

    #[test]
    fn clock_adapts_to_length() {
        let s = RATE as u64;
        assert_eq!(clock(s * 3 / 2, 4 * s), "0:01.5");
        assert_eq!(clock(83 * s, 200 * s), "1:23");
        assert_eq!(clock(3723 * s, 4000 * s), "1:02:03");
    }
}

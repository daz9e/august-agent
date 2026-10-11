//! The wizard in a real terminal: menus and inputs redrawn in place (raw mode, no alternate
//! screen), so what was answered stays above as one line each.

use super::{Answer, STEPS, Say, Ui};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use unicode_width::UnicodeWidthChar;

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const ACCENT: &str = "\x1b[38;5;141m";
const GREEN: &str = "\x1b[38;5;114m";
const RED: &str = "\x1b[38;5;203m";
const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// Options shown at once; the rest scroll.
const VIEW: usize = 8;

#[derive(Default)]
pub struct Term {
    spinner: Mutex<Option<(Arc<AtomicBool>, std::thread::JoinHandle<()>)>>,
}

/// Raw mode with the cursor hidden, while a prompt is open.
struct Raw;

impl Raw {
    fn on() -> Raw {
        terminal::enable_raw_mode().ok();
        print!("\x1b[?25l");
        Raw
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        print!("\x1b[?25h");
        std::io::stdout().flush().ok();
        terminal::disable_raw_mode().ok();
    }
}

/// `s` cut to `cols` columns.
fn fit(s: &str, cols: usize) -> String {
    let mut width = 0;
    let mut out = String::new();
    for c in s.chars() {
        width += c.width().unwrap_or(0);
        if width + 1 > cols {
            out.push('…');
            break;
        }
        out.push(c);
    }
    out
}

fn cols() -> usize {
    terminal::size().map(|(c, _)| c as usize).unwrap_or(80).max(20) - 6
}

/// The block of `rows` lines drawn last is replaced by `lines` (each one row: fitted).
fn redraw(rows: &mut usize, lines: &[String]) {
    let mut out = std::io::stdout();
    if *rows > 0 {
        write!(out, "\x1b[{rows}F").ok();
    }
    write!(out, "\x1b[J").ok();
    for line in lines {
        write!(out, "{line}\r\n").ok();
    }
    out.flush().ok();
    *rows = lines.len();
}

/// The next key pressed; `None` once `stop` is set. A resize comes as `Null` (redraw).
fn key(stop: &AtomicBool) -> Option<KeyEvent> {
    loop {
        if stop.load(Ordering::Relaxed) {
            return None;
        }
        if !event::poll(Duration::from_millis(100)).unwrap_or(false) {
            continue;
        }
        match event::read() {
            Ok(Event::Key(k)) if k.is_press() => return Some(k),
            Ok(Event::Resize(..)) => return Some(KeyEvent::new(KeyCode::Null, KeyModifiers::NONE)),
            _ => {}
        }
    }
}

fn cancels(k: &KeyEvent) -> bool {
    k.code == KeyCode::Esc || (k.modifiers.contains(KeyModifiers::CONTROL) && matches!(k.code, KeyCode::Char('c' | 'd')))
}

/// What the closed prompt leaves behind: one line, or nothing when it was `stop`ped (the
/// question went away on its own).
fn answered(rows: &mut usize, question: &str, answer: Option<&str>, stop: &AtomicBool) {
    if stop.load(Ordering::Relaxed) {
        return redraw(rows, &[]);
    }
    let line = match answer {
        Some(a) => format!("{GREEN}✔{RESET} {} {DIM}·{RESET} {ACCENT}{}{RESET}", fit(question, cols() / 2), fit(a, cols() / 2)),
        None => format!("{DIM}✖ {}{RESET}", fit(question, cols())),
    };
    redraw(rows, &[line]);
}

impl Term {
    pub fn banner(&self) {
        println!();
        println!("  {ACCENT}{BOLD}◆ August{RESET}  {DIM}setup{RESET}");
        println!("  {DIM}Your always-on personal agent. Esc pauses; `august setup` picks up again.{RESET}");
    }

    fn stop_spinner(&self) {
        if let Some((stop, thread)) = self.spinner.lock().unwrap().take() {
            stop.store(true, Ordering::Relaxed);
            thread.join().ok();
        }
    }
}

impl Ui for Term {
    fn step(&self, n: usize, title: &str) {
        self.stop_spinner();
        println!("\n{ACCENT}{BOLD}[{n}/{STEPS}]{RESET} {BOLD}{title}{RESET}");
    }

    fn say(&self, kind: Say, text: &str) {
        self.stop_spinner();
        let mark = match kind {
            Say::Info => format!("{DIM}│{RESET}"),
            Say::Ok => format!("{GREEN}✔{RESET}"),
            Say::Fail => format!("{RED}✖{RESET}"),
        };
        for (i, line) in text.lines().enumerate() {
            println!("{} {line}", if i == 0 { mark.as_str() } else { " " });
        }
    }

    fn busy(&self, text: Option<&str>) {
        self.stop_spinner();
        let Some(text) = text.map(String::from) else { return };
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::spawn(move || {
            let mut out = std::io::stdout();
            for frame in SPINNER.iter().cycle() {
                if flag.load(Ordering::Relaxed) {
                    break;
                }
                write!(out, "\r\x1b[2K{ACCENT}{frame}{RESET} {text}…").ok();
                out.flush().ok();
                std::thread::sleep(Duration::from_millis(80));
            }
            write!(out, "\r\x1b[2K").ok();
            out.flush().ok();
        });
        *self.spinner.lock().unwrap() = Some((stop, thread));
    }

    fn select(&self, question: &str, options: &[String], custom: bool, stop: &AtomicBool) -> Option<Answer> {
        self.stop_spinner();
        let _raw = Raw::on();
        let (mut filter, mut at, mut top, mut rows) = (String::new(), 0usize, 0usize, 0usize);
        let answer = loop {
            let lower = filter.to_lowercase();
            let shown: Vec<usize> = (0..options.len()).filter(|&i| options[i].to_lowercase().contains(&lower)).collect();
            at = at.min(shown.len().saturating_sub(1));
            top = top.min(at).max((at + 1).saturating_sub(VIEW));
            let w = cols();
            let mut lines = vec![format!("{ACCENT}?{RESET} {BOLD}{}{RESET}", fit(question, w))];
            if options.len() > VIEW || custom {
                lines.push(format!("  {DIM}filter ›{RESET} {filter}{ACCENT}▏{RESET}"));
            }
            for (row, &i) in shown.iter().enumerate().skip(top).take(VIEW) {
                let label = fit(&options[i], w);
                lines.push(if row == at { format!("  {ACCENT}❯ {BOLD}{label}{RESET}") } else { format!("    {label}") });
            }
            if shown.is_empty() {
                let none = if custom && !filter.is_empty() { format!("Enter uses “{filter}”") } else { "Nothing matches".into() };
                lines.push(format!("    {DIM}{none}{RESET}"));
            }
            let more = if shown.len() > VIEW { format!("{}–{} of {} · ", top + 1, (top + VIEW).min(shown.len()), shown.len()) } else { String::new() };
            let typing = if options.len() > VIEW || custom { "type to filter · " } else { "" };
            lines.push(format!("  {DIM}{more}↑↓ move · {typing}enter select · esc pause{RESET}"));
            redraw(&mut rows, &lines);
            let Some(k) = key(stop) else { break None };
            match k.code {
                _ if cancels(&k) => break None,
                KeyCode::Up => at = at.saturating_sub(1),
                KeyCode::Down if at + 1 < shown.len() => at += 1,
                KeyCode::PageUp => at = at.saturating_sub(VIEW),
                KeyCode::PageDown => at = (at + VIEW).min(shown.len().saturating_sub(1)),
                KeyCode::Enter => match shown.get(at) {
                    Some(&i) => break Some(Answer::Pick(i)),
                    None if custom && !filter.is_empty() => break Some(Answer::Typed(filter.trim().to_string())),
                    None => {}
                },
                KeyCode::Backspace => {
                    filter.pop();
                }
                KeyCode::Char(c) if options.len() > VIEW || custom => {
                    filter.push(c);
                    at = 0;
                }
                _ => {}
            }
        };
        let shown = match &answer {
            Some(Answer::Pick(i)) => Some(options[*i].as_str()),
            Some(Answer::Typed(t)) => Some(t.as_str()),
            None => None,
        };
        answered(&mut rows, question, shown, stop);
        answer
    }

    fn input(&self, question: &str, secret: bool, stop: &AtomicBool) -> Option<String> {
        self.stop_spinner();
        let _raw = Raw::on();
        let (mut text, mut rows) = (String::new(), 0usize);
        let answer = loop {
            let w = cols();
            let shown = if secret { "•".repeat(text.chars().count()) } else { text.clone() };
            // The end of a long answer, where the typing is.
            let tail: String = shown.chars().rev().take(w).collect::<Vec<_>>().into_iter().rev().collect();
            let lines = [
                format!("{ACCENT}?{RESET} {BOLD}{}{RESET}", fit(question, w)),
                format!("  {ACCENT}›{RESET} {tail}{ACCENT}▏{RESET}"),
                format!("  {DIM}enter confirm · esc pause{RESET}"),
            ];
            redraw(&mut rows, &lines);
            let Some(k) = key(stop) else { break None };
            match k.code {
                _ if cancels(&k) => break None,
                KeyCode::Enter if !text.trim().is_empty() => break Some(text.trim().to_string()),
                KeyCode::Backspace => {
                    text.pop();
                }
                KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => text.clear(),
                KeyCode::Char(c) => text.push(c),
                _ => {}
            }
        };
        let shown = answer.as_ref().map(|a| if secret { "received".to_string() } else { a.clone() });
        answered(&mut rows, question, shown.as_deref(), stop);
        answer
    }

    fn summary(&self, rows: &[(&str, String)]) {
        self.stop_spinner();
        let w = cols().min(64);
        let key = rows.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
        let lines: Vec<String> = rows.iter().map(|(k, v)| format!("{k:<key$}   {}", fit(v, w - key - 3))).collect();
        let inner = lines.iter().map(|l| l.chars().map(|c| c.width().unwrap_or(0)).sum::<usize>()).max().unwrap_or(0).max(24);
        let pad = |s: &str| inner - s.chars().map(|c| c.width().unwrap_or(0)).sum::<usize>();
        let title = "August is set up";
        println!();
        println!("{GREEN}╭{}╮{RESET}", "─".repeat(inner + 4));
        println!("{GREEN}│{RESET}  {GREEN}{BOLD}✔ {title}{RESET}{}  {GREEN}│{RESET}", " ".repeat(inner - title.len() - 2));
        println!("{GREEN}│{RESET}{}{GREEN}│{RESET}", " ".repeat(inner + 4));
        for l in &lines {
            let (k, v) = l.split_at(key);
            println!("{GREEN}│{RESET}  {DIM}{k}{RESET}{v}{}  {GREEN}│{RESET}", " ".repeat(pad(l)));
        }
        println!("{GREEN}╰{}╯{RESET}", "─".repeat(inner + 4));
    }
}

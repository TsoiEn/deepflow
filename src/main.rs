//! Mermaid live preview: edit on the left, rendered diagram on the right.
//!
//! Rendering is done by a Node sidecar holding one Puppeteer page alive
//! (see `sidecar/render.mjs`); this process only talks JSON lines to it.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine;
use image::DynamicImage;
use ratatui::Terminal;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Paragraph, Wrap};
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::Protocol;
use ratatui_image::{FilterType, Image, Resize};
use tui_textarea::TextArea;

/// Idle time before a keystroke turns into a render. This dominates how laggy the preview
/// feels, and a render only costs 65-160ms, so there is no reason to wait 300ms for it.
/// `in_flight` is what actually stops renders piling up.
const DEBOUNCE: Duration = Duration::from_millis(120);

const SAMPLE: &str = "\
flowchart TD
    A[Edit on the left] --> B[See it on the right]
    B --> C{Readable?}
    C -->|yes| D[Ship it]
    C -->|no| A
";

enum Reply {
    Png(Vec<u8>),
    Error { msg: String, line: Option<usize> },
}

fn spawn_sidecar() -> std::io::Result<(ChildStdin, Receiver<Reply>, Child)> {
    let script = std::env::var("DEEPFLOW_SIDECAR")
        .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/sidecar/render.mjs").into());
    let mut child = Command::new("node")
        .arg(script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    let stdin = child.stdin.take().expect("piped");
    let stdout = child.stdout.take().expect("piped");

    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            let reply = if v["ok"].as_bool() == Some(true) {
                match base64::engine::general_purpose::STANDARD
                    .decode(v["png"].as_str().unwrap_or_default())
                {
                    Ok(bytes) => Reply::Png(bytes),
                    Err(e) => Reply::Error {
                        msg: e.to_string(),
                        line: None,
                    },
                }
            } else {
                Reply::Error {
                    msg: v["error"].as_str().unwrap_or("render failed").to_string(),
                    line: v["line"].as_u64().map(|n| n as usize),
                }
            };
            if tx.send(reply).is_err() {
                break;
            }
        }
    });
    Ok((stdin, rx, child))
}

/// Mermaid errors are several lines of caret-art; a status bar wants one line.
fn collapse(msg: &str) -> String {
    msg.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Detecting terminal graphics support means writing a query to the terminal and reading
/// the reply off stdin. ratatui-image does that on a thread that blocks in `read()` forever
/// when the terminal never replies, and that thread then swallows every keystroke the app
/// should have received. Running the probe as a short-lived child process means its stuck
/// reader dies with the child.
const PROBE_FLAG: &str = "--probe-graphics";

/// Runs as the child: stdin and stdout are the terminal, so the answer goes out on stderr.
fn probe_graphics() -> ! {
    let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::from_fontsize((8, 16)));
    let (w, h) = picker.font_size();
    eprintln!("{:?} {w} {h}", picker.protocol_type());
    // The stuck reader may still hold the terminal in raw mode.
    let _ = disable_raw_mode();
    std::process::exit(0);
}

/// Inside tmux, pixel protocols need `allow-passthrough on`. Without it tmux swallows the
/// image data while the placeholder cells pass through as plain text, so the terminal draws
/// rows of literal placeholder glyphs. Blocky-but-readable beats that.
fn tmux_passthrough_off() -> bool {
    if std::env::var_os("TMUX").is_none() {
        return false;
    }
    Command::new("tmux")
        .args(["show", "-gv", "allow-passthrough"])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "off")
}

fn detect_picker() -> Picker {
    // shortcut: this carries the protocol and font size across, but not the extra
    // capabilities (e.g. kitty compression). Pass those too if a terminal needs them.
    let probe = || -> std::io::Result<(ProtocolType, u16, u16)> {
        let mut child = Command::new(std::env::current_exe()?)
            .arg(PROBE_FLAG)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut reply = String::new();
        child
            .stderr
            .take()
            .expect("piped")
            .read_to_string(&mut reply)?;
        child.wait()?;
        let mut parts = reply.split_whitespace();
        let proto = match parts.next() {
            Some("Kitty") => ProtocolType::Kitty,
            Some("Sixel") => ProtocolType::Sixel,
            Some("Iterm2") => ProtocolType::Iterm2,
            _ => ProtocolType::Halfblocks,
        };
        let (Some(Ok(w)), Some(Ok(h))) =
            (parts.next().map(str::parse), parts.next().map(str::parse))
        else {
            return Err(std::io::ErrorKind::InvalidData.into());
        };
        Ok((proto, w, h))
    };

    match probe() {
        Ok((proto, w, h)) => {
            let mut picker = Picker::from_fontsize((w, h));
            picker.set_protocol_type(if tmux_passthrough_off() {
                ProtocolType::Halfblocks
            } else {
                proto
            });
            picker
        }
        Err(_) => Picker::from_fontsize((8, 16)),
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().any(|a| a == PROBE_FLAG) {
        probe_graphics();
    }

    let path = std::env::args().nth(1).map(PathBuf::from);
    let text = match &path {
        Some(p) if p.exists() => std::fs::read_to_string(p)?,
        _ => SAMPLE.to_string(),
    };

    // Has to happen before the alternate screen: the probe talks to the terminal.
    let picker = detect_picker();

    let (mut sidecar_in, replies, mut child) = spawn_sidecar()?;

    let mut textarea = TextArea::from(text.lines());
    textarea.set_line_number_style(Style::new().fg(Color::DarkGray));
    textarea.set_cursor_line_style(Style::default());

    enable_raw_mode()?;
    execute!(std::io::stdout(), EnterAlternateScreen)?;
    let mut term = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;

    let res = run(
        &mut term,
        &picker,
        &mut textarea,
        &mut sidecar_in,
        &replies,
        path.as_deref(),
    );

    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen)?;
    term.show_cursor()?;
    drop(sidecar_in); // closing stdin is how the sidecar is told to shut down
    let _ = child.wait();
    res
}

fn run<B: Backend>(
    term: &mut Terminal<B>,
    picker: &Picker,
    textarea: &mut TextArea,
    sidecar_in: &mut ChildStdin,
    replies: &Receiver<Reply>,
    path: Option<&std::path::Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let title = path
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "[scratch]".into());

    let preview_title =
        if picker.protocol_type() == ProtocolType::Halfblocks && tmux_passthrough_off() {
            " preview — halfblocks: tmux allow-passthrough is off "
        } else {
            " preview "
        };

    let mut image: Option<DynamicImage> = None; // last good render, kept while the next is in flight
    let mut proto: Option<Protocol> = None;
    let mut proto_key: Option<(u64, Rect)> = None;
    let mut generation = 0u64;
    let mut error: Option<(String, Option<usize>)> = None;
    let mut saved: Option<String> = None;
    let mut in_flight = false;
    let mut dirty = true;
    let mut last_edit = Instant::now() - DEBOUNCE; // render the initial buffer at once

    loop {
        loop {
            match replies.try_recv() {
                Ok(Reply::Png(bytes)) => {
                    in_flight = false;
                    match image::load_from_memory(&bytes) {
                        Ok(img) => {
                            image = Some(img);
                            generation += 1;
                            error = None;
                        }
                        Err(e) => error = Some((e.to_string(), None)),
                    }
                }
                Ok(Reply::Error { msg, line }) => {
                    in_flight = false;
                    error = Some((msg, line));
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    return Err("render sidecar exited (is node installed?)".into());
                }
            }
        }

        if dirty && !in_flight && last_edit.elapsed() >= DEBOUNCE {
            let src = textarea.lines().join("\n");
            writeln!(sidecar_in, "{}", serde_json::json!({ "src": src }))?;
            sidecar_in.flush()?;
            dirty = false;
            in_flight = true;
        }

        let size = term.size()?;
        let [body, footer] = Layout::vertical([
            Constraint::Min(0),
            Constraint::Length(if error.is_some() { 4 } else { 1 }),
        ])
        .areas(Rect::new(0, 0, size.width, size.height));
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                .areas(body);
        let preview = Block::bordered().inner(right);

        if let Some(img) = &image
            && proto_key != Some((generation, preview))
            && preview.width > 0
            && preview.height > 0
        {
            match // Fit defaults to Nearest, which turns downscaled diagram text to mush for ~3ms less.
                picker.new_protocol(
                    img.clone(),
                    preview,
                    Resize::Fit(Some(FilterType::Triangle)),
                ) {
                Ok(p) => proto = Some(p),
                // A preview that silently stays empty is worse than a visible complaint.
                Err(e) => error = Some((format!("preview: {e}"), None)),
            }
            proto_key = Some((generation, preview));
        }

        let status = match (&error, in_flight) {
            // shortcut: the row is named, not marked in the gutter — tui-textarea keeps its
            // scroll offset private. Mark it inline if diagrams outgrow one screen.
            (Some((msg, line)), _) => {
                let where_ = line
                    .map(|n| format!("line {}: ", n.clamp(1, textarea.lines().len())))
                    .unwrap_or_default();
                Paragraph::new(format!("{where_}{}", collapse(msg)))
                    .style(Style::new().fg(Color::Red))
                    .wrap(Wrap { trim: true })
            }
            (None, true) => Paragraph::new("rendering…").style(Style::new().fg(Color::DarkGray)),
            (None, false) => {
                Paragraph::new(saved.clone().unwrap_or_else(|| "^S save   ^Q quit".into()))
                    .style(Style::new().fg(Color::DarkGray))
            }
        };

        term.draw(|f| {
            f.render_widget(
                Block::bordered().title(format!(" {title}{} ", if dirty { " •" } else { "" })),
                left,
            );
            f.render_widget(&*textarea, Block::bordered().inner(left));
            f.render_widget(Block::bordered().title(preview_title), right);
            match &proto {
                Some(p) => f.render_widget(Image::new(p), preview),
                None => f.render_widget(
                    Paragraph::new("no diagram yet").style(Style::new().fg(Color::DarkGray)),
                    preview,
                ),
            }
            f.render_widget(status, footer);
        })?;

        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        match event::read()? {
            Event::Key(KeyEvent {
                code: KeyCode::Char('q' | 'c'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }) => return Ok(()),
            Event::Key(KeyEvent {
                code: KeyCode::Char('s'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }) => {
                saved = Some(match path {
                    Some(p) => match std::fs::write(p, textarea.lines().join("\n") + "\n") {
                        Ok(()) => format!("saved {}", p.display()),
                        Err(e) => format!("save failed: {e}"),
                    },
                    None => "no file to save to (open with: deepflow file.mmd)".into(),
                });
            }
            Event::Key(key) if textarea.input(key) => {
                dirty = true;
                last_edit = Instant::now();
                saved = None;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui_image::picker::ProtocolType;

    /// The whole pipeline minus the real terminal: sidecar -> png -> protocol -> cells.
    #[test]
    fn renders_a_diagram_into_the_preview_pane() {
        let (mut stdin, replies, mut child) = spawn_sidecar().expect("node and the sidecar");
        writeln!(
            stdin,
            "{}",
            serde_json::json!({ "src": "flowchart TD\n A-->B" })
        )
        .unwrap();
        stdin.flush().unwrap();

        // First render includes booting Chrome.
        let bytes = match replies
            .recv_timeout(Duration::from_secs(60))
            .expect("a reply")
        {
            Reply::Png(bytes) => bytes,
            Reply::Error { msg, .. } => panic!("sidecar refused a valid diagram: {msg}"),
        };
        let img = image::load_from_memory(&bytes).expect("a decodable png");

        // Halfblocks is the fallback protocol: plain cells, so no real terminal is needed.
        let mut picker = Picker::from_fontsize((8, 16));
        picker.set_protocol_type(ProtocolType::Halfblocks);
        let area = Rect::new(0, 0, 40, 20);
        let proto = picker
            .new_protocol(img, area, Resize::Fit(None))
            .expect("a protocol");

        let mut term = Terminal::new(TestBackend::new(40, 20)).unwrap();
        term.draw(|f| f.render_widget(Image::new(&proto), area))
            .unwrap();

        let painted = term
            .backend()
            .buffer()
            .content()
            .iter()
            .filter(|c| c.symbol() != " ")
            .count();
        assert!(
            painted > 50,
            "preview came out blank: only {painted} cells painted"
        );

        drop(stdin);
        let _ = child.wait();
    }

    #[test]
    fn collapse_flattens_mermaid_caret_art() {
        assert_eq!(
            collapse("Parse error on line 4:\n  A-->\n------^\nExpecting 'SEMI'"),
            "Parse error on line 4: A--> ------^ Expecting 'SEMI'"
        );
    }
}

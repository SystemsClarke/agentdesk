//! The console end of the window: one frame is one write.
//!
//! `ratatui::init()` hands crossterm `std::io::stdout()`, which is a `LineWriter` with a 1 KiB buffer: a frame of a few KiB goes to the
//! console in several writes, and the console shows the screen between them (tearing, and on Windows a console round trip each). Here
//! the whole frame is built in memory, wrapped in the synchronized-update markers (terminals that know them, Windows Terminal among
//! them, show the frame at once; the others ignore them), and handed over with a single `write_all`.

use super::App;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use ratatui::crossterm::QueueableCommand;
use ratatui::Terminal;
use std::cell::Cell;
use std::io::{self, Write};
use std::rc::Rc;

/// Collects what is written and passes it on in one `write_all`, on an explicit flush while not `hold`ing, or when it grows past `LIMIT`.
pub struct FrameSink<W: Write> {
    buf: Vec<u8>,
    out: W,
    hold: Rc<Cell<bool>>,
}

const LIMIT: usize = 1 << 20;

impl<W: Write> FrameSink<W> {
    fn send(&mut self) -> io::Result<()> {
        if !self.buf.is_empty() {
            self.out.write_all(&self.buf)?;
            self.buf.clear();
        }
        self.out.flush()
    }
}

impl<W: Write> Write for FrameSink<W> {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(b);
        if self.buf.len() >= LIMIT && !self.hold.get() {
            self.send()?;
        }
        Ok(b.len())
    }

    /// ratatui flushes inside `draw`; while a frame is being built that must not reach the console yet.
    fn flush(&mut self) -> io::Result<()> {
        if self.hold.get() {
            Ok(())
        } else {
            self.send()
        }
    }
}

pub struct Console<W: Write> {
    term: Terminal<CrosstermBackend<FrameSink<W>>>,
    hold: Rc<Cell<bool>>,
}

impl<W: Write> Console<W> {
    pub fn new(out: W) -> io::Result<Console<W>> {
        let hold = Rc::new(Cell::new(false));
        let sink = FrameSink { buf: Vec::with_capacity(32 * 1024), out, hold: hold.clone() };
        Ok(Console { term: Terminal::new(CrosstermBackend::new(sink))?, hold })
    }

    /// Like `Console::new` with the screen's size given instead of asked of the terminal (a test or the bench has none).
    pub fn with_size(out: W, w: u16, h: u16) -> io::Result<Console<W>> {
        let hold = Rc::new(Cell::new(false));
        let sink = FrameSink { buf: Vec::with_capacity(32 * 1024), out, hold: hold.clone() };
        let viewport = ratatui::Viewport::Fixed(ratatui::layout::Rect::new(0, 0, w, h));
        Ok(Console { term: Terminal::with_options(CrosstermBackend::new(sink), ratatui::TerminalOptions { viewport })?, hold })
    }

    /// Draws the app and sends the frame as one write.
    pub fn draw(&mut self, app: &mut App) -> io::Result<()> {
        self.hold.set(true);
        let built = (|| -> io::Result<()> {
            self.term.backend_mut().queue(BeginSynchronizedUpdate)?;
            self.term.draw(|f| app.draw(f))?;
            self.term.backend_mut().queue(EndSynchronizedUpdate)?;
            Ok(())
        })();
        self.hold.set(false);
        built?;
        self.term.backend_mut().flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Counts `write` calls that reach the sink.
    struct Tally(Rc<Cell<usize>>);
    impl Write for Tally {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.0.set(self.0.get() + 1);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_frame_is_one_write_however_often_ratatui_flushes() {
        let n = Rc::new(Cell::new(0));
        let hold = Rc::new(Cell::new(true));
        let mut sink = FrameSink { buf: vec![], out: Tally(n.clone()), hold: hold.clone() };
        for _ in 0..50 {
            sink.write_all(&[b'x'; 500]).unwrap();
            sink.flush().unwrap();
        }
        assert_eq!(n.get(), 0, "nothing leaves while the frame is being built");
        hold.set(false);
        sink.flush().unwrap();
        assert_eq!(n.get(), 1);
    }
}

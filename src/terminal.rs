use std::io::{IsTerminal, Stdout, stdout};

use crossterm::cursor::{Hide, Show};
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::{Error, Result};

pub type AppTerminal = Terminal<CrosstermBackend<Stdout>>;

pub fn ensure_interactive(feature: &str) -> Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err(Error::InvalidRequest(format!(
            "`{feature}` requires an interactive terminal"
        )));
    }
    Ok(())
}

pub struct TerminalSession {
    pub terminal: AppTerminal,
    mouse: bool,
}

impl TerminalSession {
    pub fn enter(mouse: bool) -> Result<Self> {
        enable_raw_mode()?;
        let mut output = stdout();
        let screen_result = if mouse {
            execute!(output, EnterAlternateScreen, Hide, EnableMouseCapture)
        } else {
            execute!(output, EnterAlternateScreen, Hide)
        };
        if let Err(error) = screen_result {
            let _ = disable_raw_mode();
            let mut output = stdout();
            if mouse {
                let _ = execute!(output, DisableMouseCapture, LeaveAlternateScreen, Show);
            } else {
                let _ = execute!(output, LeaveAlternateScreen, Show);
            }
            return Err(error.into());
        }
        match Terminal::new(CrosstermBackend::new(output)) {
            Ok(terminal) => Ok(Self { terminal, mouse }),
            Err(error) => {
                let _ = disable_raw_mode();
                let mut output = stdout();
                if mouse {
                    let _ = execute!(output, DisableMouseCapture, LeaveAlternateScreen, Show);
                } else {
                    let _ = execute!(output, LeaveAlternateScreen, Show);
                }
                Err(error.into())
            }
        }
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        if self.mouse {
            let _ = execute!(
                self.terminal.backend_mut(),
                DisableMouseCapture,
                LeaveAlternateScreen,
                Show
            );
        } else {
            let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen, Show);
        }
    }
}
